use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};

use super::*;

fn assert_engine_traits<T: Clone + Send + Sync + 'static>() {}
fn assert_driver_traits<T: Future<Output = Result<()>> + Send + 'static>() {}

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
    assert_driver_traits::<RdmaEngineDriver>();

    let _: fn(&RdmaEngine) -> RdmaEngineDiagnostics = RdmaEngine::diagnostics;
    let _: Result<(RdmaEngine, RdmaEngineDriver)> =
        Err(Error::InvalidConfig("signature check".into()));

    let config = RdmaConnectionConfig::default();
    assert_eq!(config.maximum_send_work_requests(), 19);
    assert_eq!(config.maximum_receive_work_requests(), 34);

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
