use std::sync::{Arc, Barrier};

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::{CompletionMode, Error, RdmaEngineBuilder, RdmaEngineLifecycle, Result};
use rdma_io_tests::test_helpers::has_software_rdma;

fn software_device_name() -> String {
    RdmaCmDeviceList::new()
        .unwrap()
        .device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
        .expect("software RDMA device")
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

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn compact_diagnostics_cover_lifecycle_safety_and_terminal_failure() {
    if !has_software_rdma() {
        return;
    }
    for mode in [CompletionMode::Readiness, CompletionMode::Polling] {
        compact_snapshot_is_concurrent_and_terminal(mode).await;
        compact_snapshot_preserves_terminal_error(mode).await;
    }
}
