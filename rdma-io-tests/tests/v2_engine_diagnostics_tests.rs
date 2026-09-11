use std::future::Future;
use std::sync::{Arc, Barrier};
use std::task::Poll;
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, RdmaConnection, RdmaEngine, RdmaEngineBuilder,
    RdmaEngineLifecycle, RdmaListener, RdmaListenerConfig, Result,
};
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

fn software_device_name() -> String {
    RdmaCmDeviceList::new()
        .unwrap()
        .device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
        .expect("software RDMA device")
}

async fn listen_with_retry(engine: &RdmaEngine) -> RdmaListener {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match engine
                .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
                .await
            {
                Ok(listener) => return listener,
                Err(Error::Verbs(error)) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => panic!("engine listener setup failed: {error}"),
            }
        }
    })
    .await
    .expect("engine listener remained busy")
}

async fn accept_pair(
    listener: &RdmaListener,
    client_engine: &RdmaEngine,
) -> (RdmaConnection, RdmaConnection) {
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    tokio::time::timeout(Duration::from_secs(30), async {
        let (server, client) = tokio::join!(listener.accept(), client_engine.connect(address));
        (server.unwrap(), client.unwrap())
    })
    .await
    .expect("engine diagnostics pair establishment timed out")
}

async fn wait_until(description: &'static str, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if predicate() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"));
}

async fn compact_snapshot_is_concurrent_and_terminal(mode: CompletionMode) {
    let (engine, driver) = RdmaEngineBuilder::new(software_device_name())
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let initial = engine.diagnostics();
    assert_eq!(initial.lifecycle, RdmaEngineLifecycle::Created);
    assert_eq!(initial.terminal_error, None);
    assert_eq!(initial.live_connections, 0);
    assert_eq!(initial.registered_operations, 0);
    assert_eq!(initial.accepted_operations, 0);
    assert_eq!(initial.pending_reclamations, 0);
    assert_eq!(initial.available_cq_credits, 64);
    assert_eq!(initial.retained_cq_credits, 0);
    assert_eq!(initial.quarantined_operations, 0);
    assert_eq!(initial.quarantined_mrs, 0);
    assert_eq!(initial.quarantined_bytes, 0);
    assert_eq!(initial.quarantined_connections, 0);

    let start = Arc::new(Barrier::new(9));
    let readers = (0..8)
        .map(|_| {
            let engine = engine.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                engine.diagnostics()
            })
        })
        .collect::<Vec<_>>();
    start.wait();
    for reader in readers {
        assert_eq!(reader.join().unwrap(), initial);
    }

    let driver = tokio::spawn(driver);
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
    let terminal = engine.diagnostics();
    assert_eq!(terminal.lifecycle, RdmaEngineLifecycle::Terminated);
    assert_eq!(terminal.terminal_error, None);
    assert_eq!(terminal.live_connections, 0);
    assert_eq!(terminal.registered_operations, 0);
}

async fn compact_snapshot_preserves_terminal_error(mode: CompletionMode) {
    let (engine, driver) = RdmaEngineBuilder::new(software_device_name())
        .completion_mode(mode)
        .build()
        .unwrap();
    engine
        .test_resources()
        .unwrap()
        .inject_driver_failure(Error::InvalidConfig("injected compact failure".into()))
        .unwrap();
    let result: Result<()> = tokio::spawn(driver).await.unwrap();
    assert!(matches!(
        result,
        Err(Error::InvalidConfig(message)) if message == "injected compact failure"
    ));
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Failed);
    let error = diagnostics.terminal_error.expect("terminal error summary");
    assert_eq!(error.class, "InvalidConfig");
    assert!(error.message.contains("injected compact failure"));
}

async fn compact_snapshot_is_published_on_synchronous_driver_drop(mode: CompletionMode) {
    let (engine, driver) = RdmaEngineBuilder::new(software_device_name())
        .completion_mode(mode)
        .build()
        .unwrap();

    drop(driver);

    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Failed);
    let error = diagnostics
        .terminal_error
        .expect("driver-drop terminal error");
    assert_eq!(error.class, "DriverShutdown");
    assert_eq!(diagnostics.live_connections, 0);
    assert_eq!(diagnostics.registered_operations, 0);
    assert_eq!(diagnostics.accepted_operations, 0);
    assert_eq!(diagnostics.quarantined_connections, 0);
}

async fn compact_snapshot_captures_retained_quarantine(mode: CompletionMode) {
    let device = software_device_name();
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .connection_drain_deadline(Duration::from_millis(50))
        .shutdown_deadline(Duration::from_millis(200))
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let resources = server_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = listen_with_retry(&server_engine).await;
    let (server, client) = accept_pair(&listener, &client_engine).await;
    let recv_mr = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    wait_until(
        "driver did not publish the accepted operation snapshot",
        || {
            let diagnostics = server_engine.diagnostics();
            diagnostics.registered_operations == 1
                && diagnostics.accepted_operations == 1
                && diagnostics.available_cq_credits == 63
        },
    )
    .await;

    let suppression = resources
        .suppress_next_connection_flush_cqe(&server)
        .unwrap();
    resources.fail_next_connection_qp_destroy(&server).unwrap();
    assert!(matches!(
        server.close().await,
        Err(Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));
    let (operation_error, returned_recv) = recv.await;
    assert!(matches!(operation_error, Err(Error::TransportClosed)));
    assert!(returned_recv.is_none());

    let diagnostics = server_engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Running);
    assert!(diagnostics.terminal_error.is_none());
    assert_eq!(diagnostics.live_connections, 1);
    assert_eq!(diagnostics.registered_operations, 1);
    assert_eq!(diagnostics.accepted_operations, 1);
    assert_eq!(diagnostics.available_cq_credits, 63);
    assert_eq!(diagnostics.retained_cq_credits, 1);
    assert_eq!(diagnostics.quarantined_operations, 1);
    assert_eq!(diagnostics.quarantined_mrs, 1);
    assert_eq!(diagnostics.quarantined_connections, 1);

    drop(suppression);
    assert!(matches!(
        server_engine.shutdown().await,
        Err(Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));
    assert!(matches!(
        server_task.await.unwrap(),
        Err(Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));
    let terminal = server_engine.diagnostics();
    assert_eq!(terminal.lifecycle, RdmaEngineLifecycle::Failed);
    assert!(matches!(
        terminal.terminal_error.as_ref(),
        Some(error) if error.class == "EngineWedged"
    ));
    assert_eq!(terminal.live_connections, 1);
    assert_eq!(terminal.registered_operations, 1);
    assert_eq!(terminal.accepted_operations, 1);
    assert_eq!(terminal.available_cq_credits, 63);
    assert_eq!(terminal.retained_cq_credits, 1);
    assert_eq!(terminal.quarantined_operations, 1);
    assert_eq!(terminal.quarantined_mrs, 1);
    assert_eq!(terminal.quarantined_connections, 1);
    let _ = client.close().await;
    let _ = listener.close().await;
    let _ = client_engine.shutdown().await;
    let _ = client_task.await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn compact_diagnostics_cover_lifecycle_safety_and_terminal_failure() {
    if !has_software_rdma() {
        return;
    }
    for mode in [CompletionMode::Readiness, CompletionMode::Polling] {
        compact_snapshot_is_concurrent_and_terminal(mode).await;
        compact_snapshot_preserves_terminal_error(mode).await;
        compact_snapshot_is_published_on_synchronous_driver_drop(mode).await;
        compact_snapshot_captures_retained_quarantine(mode).await;
    }
}
