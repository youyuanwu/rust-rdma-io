use std::sync::{Arc, Barrier};
use std::time::Duration;

use rdma_io::v2::{
    CompletionMode, Error, MessageTransportBuilder, RdmaEngine, RdmaEngineBuilder,
    RdmaEngineLifecycle, RdmaListenerConfig,
};
use rdma_io::wc::WcOpcode;
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

fn software_device_name() -> Option<String> {
    let list = rdma_io::cm::RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn build_engine(
    mode: CompletionMode,
    drain_deadline: Duration,
) -> (RdmaEngine, rdma_io::v2::RdmaEngineDriver) {
    RdmaEngineBuilder::new(software_device_name().expect("software RDMA device"))
        .completion_mode(mode)
        .maximum_live_connections(32)
        .maximum_inflight_operations(512)
        .cq_capacity(512)
        .cq_completion_budget(7)
        .cm_event_budget(11)
        .reclamation_budget(13)
        .ready_connection_quantum(3)
        .connection_drain_deadline(drain_deadline)
        .shutdown_deadline(Duration::from_secs(5))
        .build()
        .unwrap()
}

async fn assert_resource_and_thread_snapshots(mode: CompletionMode) {
    let (engine, driver) = build_engine(mode, Duration::from_secs(5));
    let initial = engine.diagnostics();
    assert_eq!(initial.lifecycle, RdmaEngineLifecycle::Created);
    assert_eq!(initial.terminal_error, None);
    assert_eq!(initial.completion_mode, mode);
    assert_eq!(initial.maximum_live_connections, 32);
    assert_eq!(initial.maximum_inflight_operations, 512);
    assert_eq!(initial.cq_capacity, 512);
    assert_eq!(initial.cq_completion_budget, 7);
    assert_eq!(initial.cm_event_budget, 11);
    assert_eq!(initial.reclamation_budget, 13);
    assert_eq!(initial.ready_connection_quantum, 3);
    assert_eq!(initial.shared_contexts, 1);
    assert_eq!(initial.shared_protection_domains, 1);
    assert_eq!(initial.shared_completion_queues, 1);
    assert_eq!(initial.shared_cm_event_channels, 1);
    assert_eq!(initial.shared_cm_event_fds, 1);
    assert_eq!(initial.explicit_engine_drivers, 1);
    assert_eq!(initial.library_owned_tasks, 0);
    assert_eq!(
        initial.shared_completion_channels,
        usize::from(mode == CompletionMode::Readiness)
    );
    assert_eq!(
        initial.shared_cq_notification_fds,
        usize::from(mode == CompletionMode::Readiness)
    );

    let start = Arc::new(Barrier::new(17));
    let workers = (0..16)
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
    for worker in workers {
        let diagnostics = worker.join().unwrap();
        assert_eq!(diagnostics.device_name, initial.device_name);
        assert_eq!(diagnostics.completion_mode, mode);
        assert_eq!(diagnostics.shared_completion_queues, 1);
        assert_eq!(diagnostics.explicit_engine_drivers, 1);
        assert_eq!(diagnostics.library_owned_tasks, 0);
    }

    let driver = tokio::spawn(driver);
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
    let terminal = engine.diagnostics();
    assert_eq!(terminal.lifecycle, RdmaEngineLifecycle::Terminated);
    assert_eq!(terminal.terminal_error, None);
    assert_eq!(terminal.shutdowns, 1);
}

async fn assert_terminal_error_snapshot(mode: CompletionMode) {
    let (engine, driver) = build_engine(mode, Duration::from_secs(5));
    engine
        .test_resources()
        .unwrap()
        .inject_driver_failure(Error::InvalidConfig("injected diagnostics failure".into()))
        .unwrap();
    let result = tokio::spawn(driver).await.unwrap();
    assert!(matches!(
        result,
        Err(Error::InvalidConfig(message)) if message == "injected diagnostics failure"
    ));
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Failed);
    assert_eq!(diagnostics.terminal_driver_errors, 1);
    let terminal = diagnostics.terminal_error.expect("terminal error summary");
    assert_eq!(terminal.class, "InvalidConfig");
    assert!(terminal.message.contains("injected diagnostics failure"));
}

async fn wait_for(
    engine: &RdmaEngine,
    predicate: impl Fn(&rdma_io::v2::RdmaEngineDiagnostics) -> bool,
) -> rdma_io::v2::RdmaEngineDiagnostics {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let diagnostics = engine.diagnostics();
            if predicate(&diagnostics) {
                return diagnostics;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("diagnostic condition timed out")
}

async fn assert_routing_and_quarantine_metrics(mode: CompletionMode) {
    let (engine, driver) = build_engine(mode, Duration::from_millis(100));
    let resources = engine.test_resources().unwrap();
    let driver = tokio::spawn(driver);
    let listener = engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(4),
        )
        .await
        .unwrap();
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let (server, client) = tokio::join!(
        MessageTransportBuilder::new()
            .send_buffers(2)
            .recv_buffers(2)
            .buffer_size(128)
            .accept_on(&listener),
        MessageTransportBuilder::new()
            .send_buffers(2)
            .recv_buffers(2)
            .buffer_size(128)
            .connect_on(&engine, address)
    );
    let server = server.unwrap();
    let client = client.unwrap();
    let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
    server_ready.unwrap();
    client_ready.unwrap();

    let established = wait_for(&engine, |diagnostics| {
        diagnostics.accepted_outstanding_operations == 8
            && diagnostics.connections.len() == 2
            && diagnostics.listeners.len() == 1
    })
    .await;
    assert_eq!(established.connections_admitted, 2);
    assert_eq!(established.connections_opened, 2);
    assert_eq!(established.inbound_requests_accepted, 1);
    assert_eq!(established.establishing_connection_reservations, 0);
    assert_eq!(established.established_connection_reservations, 2);
    assert_eq!(established.registered_operations, 8);
    assert_eq!(established.free_operation_slots, 504);
    assert_eq!(established.free_cq_credits, 504);
    assert!(
        established
            .connections
            .iter()
            .all(|connection| connection.accepted_outstanding_operations == 4)
    );
    assert_eq!(established.listeners[0].queued_inbound_requests, 0);
    assert_eq!(established.listeners[0].pending_accepts, 0);
    assert_eq!(established.listeners[0].selected_accepts, 0);

    let client = Arc::new(client);
    let connection = client.test_connection().unwrap();
    let held = resources
        .suppress_next_connection_cqe_with_opcode(&connection, WcOpcode::Send)
        .unwrap();
    let sending_client = Arc::clone(&client);
    let send = tokio::spawn(async move { sending_client.send(b"routed").await });
    held.wait_observed().await.unwrap();
    let identity = held.completion_identity().unwrap();
    let before_rejects = engine.diagnostics();
    resources
        .inject_completion(identity.wr_id, identity.qp_num + 1, identity.opcode)
        .unwrap();
    resources
        .inject_completion(
            identity.wr_id.wrapping_add(1_u64 << 32),
            identity.qp_num,
            identity.opcode,
        )
        .unwrap();
    resources
        .inject_completion(u64::MAX, identity.qp_num, identity.opcode)
        .unwrap();
    resources
        .inject_completion(identity.wr_id, identity.qp_num, WcOpcode::Recv)
        .unwrap();
    let rejected = engine.diagnostics();
    assert_eq!(
        rejected.wrong_qp_num_cqes,
        before_rejects.wrong_qp_num_cqes + 1
    );
    assert_eq!(
        rejected.stale_operation_cqes,
        before_rejects.stale_operation_cqes + 1
    );
    assert_eq!(rejected.unknown_cqes, before_rejects.unknown_cqes + 1);
    assert_eq!(
        rejected.unexpected_opcode_cqes,
        before_rejects.unexpected_opcode_cqes + 1
    );
    held.release().unwrap();
    send.await.unwrap().unwrap();
    let routed = server.recv().await.unwrap();
    assert_eq!(&*routed, b"routed");
    drop(routed);
    resources
        .inject_completion(identity.wr_id, identity.qp_num, identity.opcode)
        .unwrap();
    assert_eq!(
        engine.diagnostics().duplicate_cqes,
        rejected.duplicate_cqes + 1
    );

    let connection = client.test_connection().unwrap();
    let held = resources
        .suppress_next_connection_cqe_with_opcode(&connection, WcOpcode::Send)
        .unwrap();
    let sending = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.send(b"quarantine").await })
    };
    held.wait_observed().await.unwrap();
    sending.abort();
    assert!(sending.await.unwrap_err().is_cancelled());
    let close = tokio::time::timeout(Duration::from_secs(2), client.close())
        .await
        .expect("connection drain deadline did not fire");
    assert!(matches!(
        close,
        Err(Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));
    let quarantined = engine.diagnostics();
    assert_eq!(quarantined.quarantined_bundles, 1);
    assert_eq!(quarantined.quarantined_connection_reservations, 1);
    assert_eq!(quarantined.registered_quarantined_qps, 1);
    assert_eq!(quarantined.quarantined_operations, 1);
    assert_eq!(quarantined.quarantined_mrs, 1);
    assert!(quarantined.quarantined_bytes >= 128);
    assert_eq!(quarantined.retained_cq_credits, 1);
    assert!(quarantined.oldest_quarantine_age.is_some());
    assert_eq!(quarantined.connection_quarantine_outcomes, 1);
    assert!(quarantined.connections.iter().any(
        |connection| connection.quarantined && connection.accepted_outstanding_operations == 1
    ));

    held.release().unwrap();
    let recovered = wait_for(&engine, |diagnostics| {
        diagnostics.quarantined_bundles == 0
            && diagnostics.retained_cq_credits == 0
            && diagnostics.live_connection_reservations == 1
    })
    .await;
    assert!(recovered.quarantine_recoveries >= 1);
    assert_eq!(recovered.oldest_quarantine_age, None);
    assert!(recovered.operations_offered >= established.operations_offered);
    assert!(recovered.operations_completed >= established.operations_completed);
    assert!(recovered.driver_wakeups >= established.driver_wakeups);
    assert!(matches!(
        client.close().await,
        Err(Error::ConnectionQuarantined { .. })
    ));

    drop(connection);
    drop(client);
    assert!(matches!(
        server.close().await,
        Ok(()) | Err(Error::TransportClosed)
    ));
    listener.close().await.unwrap();
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
    let terminal = engine.diagnostics();
    assert_eq!(terminal.lifecycle, RdmaEngineLifecycle::Terminated);
    assert_eq!(terminal.terminal_error, None);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn capacities_resources_modes_lifecycle_and_16_thread_snapshots_are_exact() {
    if !has_software_rdma() {
        return;
    }
    assert_resource_and_thread_snapshots(CompletionMode::Readiness).await;
    assert_resource_and_thread_snapshots(CompletionMode::Polling).await;
    assert_terminal_error_snapshot(CompletionMode::Readiness).await;
    assert_terminal_error_snapshot(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn routing_quarantine_and_monotonic_counters_are_exact_in_both_modes() {
    if !has_software_rdma() {
        return;
    }
    assert_routing_and_quarantine_metrics(CompletionMode::Readiness).await;
    assert_routing_and_quarantine_metrics(CompletionMode::Polling).await;
}
