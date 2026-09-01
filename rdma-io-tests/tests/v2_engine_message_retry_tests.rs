use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::test_support::TestHelloOverride;
use rdma_io::v2::{
    CompletionMode, Error, MessageTransportBuilder, RdmaEngine, RdmaEngineBuilder, Result,
};
use rdma_io_tests::engine_test_helpers::establish_message_pair_with_retry_before_connect_accept;
use rdma_io_tests::test_helpers::has_software_rdma;

fn software_device_name() -> Option<String> {
    let list = RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn build_engine() -> (RdmaEngine, tokio::task::JoinHandle<Result<()>>) {
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Readiness)
        .maximum_live_connections(8)
        .maximum_inflight_operations(512)
        .cq_capacity(512)
        .connection_drain_deadline(Duration::from_secs(5))
        .shutdown_deadline(Duration::from_secs(5))
        .build()
        .unwrap();
    (engine, tokio::spawn(driver))
}

async fn shutdown_engine(engine: RdmaEngine, driver: tokio::task::JoinHandle<Result<()>>) {
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn transient_retry_reclaims_capacity_routes_and_requests_before_retry() {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine();
    let (client_engine, client_driver) = build_engine();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_check = Arc::clone(&attempts);
    let (listener, server, client) = establish_message_pair_with_retry_before_connect_accept(
        &server_engine,
        &client_engine,
        MessageTransportBuilder::new,
        move |attempt| {
            attempts_for_check.fetch_add(1, Ordering::AcqRel);
            if attempt == 0 {
                Err(Error::Verbs(std::io::Error::other(
                    "RDMA CM ConnectError failed with status -22 for id=0x1 listen_id=0x0",
                )))
            } else {
                Ok(())
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert_eq!(server_engine.diagnostics().live_connection_reservations, 1);
    assert_eq!(server_engine.diagnostics().listener_count, 1);
    assert_eq!(client_engine.diagnostics().live_connection_reservations, 1);

    let _ = tokio::join!(server.close(), client.close());
    listener.close().await.unwrap();
    drop(server);
    drop(client);
    drop(listener);
    for engine in [&server_engine, &client_engine] {
        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.live_connection_reservations, 0);
        assert_eq!(diagnostics.registered_operations, 0);
        assert_eq!(diagnostics.accepted_outstanding_operations, 0);
        assert!(diagnostics.connections().is_empty());
        let instrumentation = engine.test_resources().unwrap().instrumentation().unwrap();
        assert_eq!(instrumentation.cm_pending_routes, 0);
        assert_eq!(instrumentation.cm_retained_owners, 0);
    }
    assert_eq!(server_engine.diagnostics().listener_count, 0);

    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn hello_protocol_failure_is_never_retried() {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine();
    let (client_engine, client_driver) = build_engine();
    let builder_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&builder_calls);

    let result = establish_message_pair_with_retry_before_connect_accept(
        &server_engine,
        &client_engine,
        move || {
            calls.fetch_add(1, Ordering::AcqRel);
            MessageTransportBuilder::new().test_hello_override(TestHelloOverride::BadMagic)
        },
        |_| Ok(()),
    )
    .await;

    assert!(result.is_err(), "malformed HELLO unexpectedly succeeded");
    assert_eq!(
        builder_calls.load(Ordering::Acquire),
        2,
        "HELLO failure must not start a second connect/accept attempt"
    );

    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}
