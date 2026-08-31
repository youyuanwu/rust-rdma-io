use std::future::Future;

use super::*;

fn assert_engine_traits<T: Clone + Send + Sync + 'static>() {}
fn assert_driver_traits<T: Future<Output = Result<()>> + Send + 'static>() {}

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

#[tokio::test]
async fn driver_drop_wakes_shutdown_with_terminal_error() {
    let (engine, driver) = test_engine_pair(CompletionMode::Readiness);
    drop(driver);
    assert!(matches!(
        engine.shutdown().await,
        Err(Error::DriverShutdown)
    ));
    assert_eq!(
        engine
            .diagnostics()
            .terminal_error
            .expect("terminal summary")
            .class,
        "DriverShutdown"
    );
}
