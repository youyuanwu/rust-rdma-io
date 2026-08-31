use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};

use super::*;
use crate::v2::engine::connection::{WorkRequestPoster, install_connection};
use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
use crate::v2::{AccessIntent, Completion, Mr};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

fn assert_engine_traits<T: Clone + Send + Sync + 'static>() {}
fn assert_driver_traits<T: Future<Output = Result<()>> + Send + 'static>() {}
fn assert_operation_traits<
    T: Future<Output = (Result<Completion>, Option<Mr>)> + Send + 'static,
>() {
}
fn assert_connect_future<T: Future<Output = Result<RdmaConnection>> + Send>(_: T) {}
fn assert_listen_future<T: Future<Output = Result<RdmaListener>> + Send>(_: T) {}
fn assert_accept_future<T: Future<Output = Result<RdmaConnection>> + Send>(_: T) {}

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
fn exact_public_types_and_traits_compile() {
    assert_engine_traits::<RdmaEngine>();
    assert_engine_traits::<RdmaListener>();
    assert_driver_traits::<RdmaEngineDriver>();
    assert_operation_traits::<RdmaOperation>();

    let _: fn(&RdmaEngine) -> RdmaEngineDiagnostics = RdmaEngine::diagnostics;
    let _: Result<(RdmaEngine, RdmaEngineDriver)> =
        Err(Error::InvalidConfig("signature check".into()));

    let config = RdmaConnectionConfig::default();
    assert_eq!(config.maximum_send_work_requests(), 19);
    assert_eq!(config.maximum_receive_work_requests(), 34);
    assert_eq!(
        RdmaListenerConfig::default().backlog_capacity(),
        listener::DEFAULT_LISTENER_BACKLOG
    );

    let connection = Error::ConnectionQuarantined {
        outstanding_operations: 1,
        cq_debt: 1,
    };
    assert!(matches!(connection, Error::ConnectionQuarantined { .. }));
    let engine = Error::EngineWedged {
        retained_bundles: 1,
        outstanding_operations: 1,
        cq_debt: 1,
    };
    assert!(matches!(engine, Error::EngineWedged { .. }));

    let _: fn(&RdmaConnection, usize, AccessIntent) -> Result<Mr> = RdmaConnection::register_memory;
    let _: fn(&RdmaConnection, Mr, Option<(usize, usize)>) -> RdmaOperation = RdmaConnection::send;
    let _: fn(&RdmaConnection, Mr, Option<(usize, usize)>) -> RdmaOperation = RdmaConnection::recv;
    let _: fn(&RdmaConnection) -> RdmaConnectionIdentity = RdmaConnection::identity;

    fn check_connect_methods(engine: &RdmaEngine, address: std::net::SocketAddr) {
        assert_connect_future(engine.connect(address));
        assert_connect_future(engine.connect_with_config(address, RdmaConnectionConfig::default()));
        assert_listen_future(engine.listen(address, RdmaListenerConfig::default()));
    }
    fn check_accept_methods(listener: &RdmaListener) {
        assert_accept_future(listener.accept());
        assert_accept_future(listener.accept_with_config(RdmaConnectionConfig::default()));
    }
    let _ = check_connect_methods;
    let _ = check_accept_methods;
}

#[test]
fn engine_failure_preserves_explicit_cq_debt() {
    let failure = EngineFailure::from_progress(Error::EngineWedged {
        retained_bundles: 2,
        outstanding_operations: 3,
        cq_debt: 5,
    });
    assert!(matches!(
        failure.into_error(),
        Error::EngineWedged {
            retained_bundles: 2,
            outstanding_operations: 3,
            cq_debt: 5,
        }
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

    impl WorkRequestPoster for ShutdownPoster {
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

        fn destroy_qp(&self) -> bool {
            true
        }

        #[cfg(any(test, feature = "test-hooks"))]
        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    let mut connections = Vec::new();
    let mut posters = Vec::new();
    for qp_num in 1..=3 {
        let poster = Arc::new(ShutdownPoster {
            qp_num,
            error_transitions: AtomicUsize::new(0),
        });
        let connection = install_connection(
            &engine.shared,
            Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        posters.push(poster);
        connections.push(connection);
    }

    engine.shared.request_shutdown();
    engine.shared.begin_all_connection_close();
    engine.shared.begin_all_connection_close();

    assert!(
        engine
            .shared
            .shutdown_connection_close_started
            .load(Ordering::Acquire)
    );
    assert!(
        connections
            .iter()
            .all(|connection| connection.state.close_started())
    );
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
