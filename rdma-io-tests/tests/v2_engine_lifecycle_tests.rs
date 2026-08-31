use std::task::Poll;
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::test_support::destruction::{DestructionEvent, DestructionKind, DestructionRecorder};
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, RdmaConnection, RdmaEngine, RdmaEngineBuilder,
    RdmaListener, RdmaListenerConfig,
};
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

fn software_device_name() -> Option<String> {
    let list = RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

async fn accept_pair(
    listener: &RdmaListener,
    client_engine: &RdmaEngine,
) -> (RdmaConnection, RdmaConnection) {
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server, client) = tokio::join!(listener.accept(), client_engine.connect(address));
        (server.unwrap(), client.unwrap())
    })
    .await
    .expect("engine lifecycle pair establishment timed out")
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

fn assert_cm_ids_were_drained_before_destroy(events: &[DestructionEvent]) {
    for (index, event) in events.iter().enumerate() {
        if event.kind != DestructionKind::CmId {
            continue;
        }
        assert_eq!(event.result, Some(0));
        assert!(
            events[..index].iter().any(|prior| {
                prior.kind == DestructionKind::CmDrainToWouldBlock && prior.address == event.address
            }),
            "CM ID {:#x} lacked drain-to-WouldBlock evidence",
            event.address
        );
    }
}

fn count(events: &[DestructionEvent], kind: DestructionKind) -> usize {
    events.iter().filter(|event| event.kind == kind).count()
}

fn position(events: &[DestructionEvent], kind: DestructionKind) -> usize {
    events
        .iter()
        .position(|event| event.kind == kind)
        .unwrap_or_else(|| panic!("missing destruction event {kind:?}"))
}

fn assert_provider_root_drop_order(
    events: &[DestructionEvent],
    mode: CompletionMode,
    expected_engines: usize,
    expect_final_drain: bool,
) {
    assert_eq!(
        count(events, DestructionKind::CmFinalDrainToWouldBlock),
        usize::from(expect_final_drain) * expected_engines
    );
    assert_eq!(
        count(events, DestructionKind::CompletionQueue),
        expected_engines
    );
    assert_eq!(
        count(events, DestructionKind::ProtectionDomain),
        expected_engines
    );
    assert_eq!(
        count(events, DestructionKind::CmEventChannel),
        expected_engines
    );
    assert_eq!(
        count(events, DestructionKind::ContextFacade),
        expected_engines
    );
    assert_eq!(
        count(events, DestructionKind::RdmaFreeDevices),
        expected_engines
    );

    let readiness = usize::from(mode == CompletionMode::Readiness) * expected_engines;
    assert_eq!(
        count(events, DestructionKind::CqReadinessAdapter),
        readiness
    );
    assert_eq!(
        count(events, DestructionKind::CmReadinessAdapter),
        readiness
    );
    assert_eq!(count(events, DestructionKind::CompletionChannel), readiness);

    let mut final_drains = 0;
    let mut cq_adapters = 0;
    let mut cm_adapters = 0;
    let mut cqs = 0;
    let mut completion_channels = 0;
    let mut pds = 0;
    let mut cm_channels = 0;
    let mut contexts = 0;
    let mut anchors = 0;
    for event in events {
        match event.kind {
            DestructionKind::CmFinalDrainToWouldBlock => final_drains += 1,
            DestructionKind::CqReadinessAdapter => {
                cq_adapters += 1;
                if expect_final_drain {
                    assert!(cq_adapters <= final_drains);
                }
            }
            DestructionKind::CmReadinessAdapter => {
                cm_adapters += 1;
                if expect_final_drain {
                    assert!(cm_adapters <= final_drains);
                }
            }
            DestructionKind::CompletionQueue => {
                assert_eq!(event.result, Some(0));
                cqs += 1;
                if expect_final_drain {
                    assert!(cqs <= final_drains);
                }
                if mode == CompletionMode::Readiness {
                    assert!(cqs <= cq_adapters);
                }
            }
            DestructionKind::CompletionChannel => {
                assert_eq!(event.result, Some(0));
                completion_channels += 1;
                assert!(completion_channels <= cqs);
            }
            DestructionKind::ProtectionDomain => {
                assert_eq!(event.result, Some(0));
                pds += 1;
                assert!(pds <= cqs);
            }
            DestructionKind::CmEventChannel => {
                cm_channels += 1;
                if expect_final_drain {
                    assert!(cm_channels <= final_drains);
                }
                if mode == CompletionMode::Readiness {
                    assert!(cm_channels <= cm_adapters);
                }
            }
            DestructionKind::ContextFacade => {
                contexts += 1;
                assert!(contexts <= pds);
                assert!(contexts <= cm_channels);
            }
            DestructionKind::RdmaFreeDevices => {
                anchors += 1;
                assert!(anchors <= contexts);
            }
            _ => {}
        }
    }
    assert_eq!(anchors, expected_engines);
}

async fn run_clean_close(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let recorder = DestructionRecorder::arm(256);
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .build()
        .unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(2),
        )
        .await
        .unwrap();
    let (server, client) = accept_pair(&listener, &client_engine).await;

    let recv = server.register_memory(32, AccessIntent::LocalOnly).unwrap();
    let mut send = client.register_memory(32, AccessIntent::LocalOnly).unwrap();
    send.as_mut_slice()[0] = 91;
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(server.recv(recv, None), client.send(send, None));
    recv_result.unwrap();
    send_result.unwrap();
    assert_eq!(recv.unwrap().as_slice()[0], 91);
    drop(send);

    let (server_close, client_close) = tokio::join!(server.close(), client.close());
    server_close.unwrap();
    client_close.unwrap();
    assert_eq!(
        server_engine.diagnostics().accepted_outstanding_operations,
        0
    );
    assert_eq!(
        client_engine.diagnostics().accepted_outstanding_operations,
        0
    );
    assert_eq!(server_engine.diagnostics().registered_quarantined_qps, 0);
    assert_eq!(client_engine.diagnostics().registered_quarantined_qps, 0);
    assert_eq!(server_engine.diagnostics().qp_destroys, 1);
    assert_eq!(client_engine.diagnostics().qp_destroys, 1);

    listener.close().await.unwrap();
    let (server_shutdown, client_shutdown) =
        tokio::join!(server_engine.shutdown(), client_engine.shutdown());
    server_shutdown.unwrap();
    client_shutdown.unwrap();
    server_task.await.unwrap().unwrap();
    client_task.await.unwrap().unwrap();
    drop(server);
    drop(client);
    drop(listener);
    drop(server_engine);
    drop(client_engine);

    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_cm_ids_were_drained_before_destroy(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::QueuePair)
            .count(),
        2
    );
    assert!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .all(|event| event.result == Some(0))
    );
    assert_eq!(
        events.last().map(|event| event.kind),
        Some(DestructionKind::RdmaFreeDevices)
    );
    assert_provider_root_drop_order(&events, mode, 2, true);
}

async fn run_missing_cqe_recovery(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .connection_drain_deadline(Duration::from_millis(100))
        .shutdown_deadline(Duration::from_secs(5))
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let server_resources = server_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
        .await
        .unwrap();
    let (server, client) = accept_pair(&listener, &client_engine).await;

    let recv_mr = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let recv_mr_address = recv_mr.inner().as_raw() as usize;
    let mut send_mr = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    send_mr.as_mut_slice()[0] = 37;
    let recorder = DestructionRecorder::arm(128);
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let suppression = server_resources
        .suppress_next_connection_cqe(&server)
        .unwrap();
    let (send_result, returned_send) = client.send(send_mr, None).await;
    send_result.unwrap();
    let returned_send = returned_send.expect("send MR must return after its exact CQE");
    suppression.wait_observed().await.unwrap();

    let close_result = tokio::time::timeout(Duration::from_secs(3), server.close())
        .await
        .expect("connection close did not reach its configured deadline")
        .unwrap_err();
    assert!(matches!(
        close_result,
        Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));
    let (operation_result, returned_recv) = recv.await;
    assert!(matches!(operation_result, Err(Error::TransportClosed)));
    assert!(returned_recv.is_none());

    let quarantined = server_engine.diagnostics();
    assert_eq!(quarantined.registered_quarantined_qps, 1);
    assert_eq!(quarantined.quarantined_operations, 1);
    assert_eq!(quarantined.retained_cq_credits, 1);
    assert_eq!(quarantined.live_connection_reservations, 1);
    assert_eq!(quarantined.qp_destroys, 0);
    let before_recovery = recorder.snapshot();
    assert!(
        !before_recovery
            .iter()
            .any(|event| matches!(event.kind, DestructionKind::MemoryRegion)
                && event.address == recv_mr_address)
    );

    suppression.release().unwrap();
    wait_until(
        "late real CQE did not recover and retire the connection",
        || {
            let diagnostics = server_engine.diagnostics();
            diagnostics.live_connection_reservations == 0
                && diagnostics.registered_operations == 0
                && diagnostics.registered_quarantined_qps == 0
                && diagnostics.qp_destroys == 1
        },
    )
    .await;
    assert_eq!(server_engine.diagnostics().quarantine_recoveries, 1);
    assert!(recorder.snapshot().iter().any(|event| {
        event.kind == DestructionKind::MemoryRegion
            && event.address == recv_mr_address
            && event.result == Some(0)
    }));
    let repeated = server.close().await.unwrap_err();
    assert!(matches!(
        repeated,
        Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));

    drop(returned_send);
    client.close().await.unwrap();
    listener.close().await.unwrap();
    let (server_shutdown, client_shutdown) =
        tokio::join!(server_engine.shutdown(), client_engine.shutdown());
    server_shutdown.unwrap();
    client_shutdown.unwrap();
    server_task.await.unwrap().unwrap();
    client_task.await.unwrap().unwrap();
    drop(server_resources);
    drop(server);
    drop(client);
    drop(listener);
    drop(server_engine);
    drop(client_engine);

    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_cm_ids_were_drained_before_destroy(&events);
}

async fn run_shutdown_wedge(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .connection_drain_deadline(Duration::from_millis(50))
        .shutdown_deadline(Duration::from_millis(200))
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let server_resources = server_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
        .await
        .unwrap();
    let (server, client) = accept_pair(&listener, &client_engine).await;

    let recv_mr = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let send_mr = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let suppression = server_resources
        .suppress_next_connection_cqe(&server)
        .unwrap();
    let (send_result, returned_send) = client.send(send_mr, None).await;
    send_result.unwrap();
    suppression.wait_observed().await.unwrap();

    let shutdown_error = tokio::time::timeout(Duration::from_secs(3), server_engine.shutdown())
        .await
        .expect("engine shutdown did not reach its configured wedge deadline")
        .unwrap_err();
    assert!(matches!(
        shutdown_error,
        Error::EngineWedged {
            retained_bundles: 1..,
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));
    let driver_error = server_task.await.unwrap().unwrap_err();
    assert_eq!(driver_error.to_string(), shutdown_error.to_string());
    let (operation_error, returned_recv) = recv.await;
    assert_eq!(
        operation_error.unwrap_err().to_string(),
        shutdown_error.to_string()
    );
    assert!(returned_recv.is_none());
    let diagnostics = server_engine.diagnostics();
    assert_eq!(
        diagnostics.terminal_error.unwrap().message,
        shutdown_error.to_string()
    );
    assert_eq!(diagnostics.registered_quarantined_qps, 1);
    assert_eq!(diagnostics.qp_destroys, 0);
    assert_eq!(diagnostics.engine_wedges, 1);
    drop(suppression);

    drop(returned_send);
    let _ = client.close().await;
    let _ = listener.close().await;
    let _ = client_engine.shutdown().await;
    let _ = client_task.await;
    drop(server_resources);
    drop(server);
    drop(client);
    drop(listener);
    drop(server_engine);
    drop(client_engine);
}

async fn run_unspawned_driver_drop(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    drop(driver);
    assert!(matches!(
        engine.shutdown().await,
        Err(Error::DriverShutdown)
    ));
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.shutdowns, 0);
    let terminal = diagnostics.terminal_error.unwrap();
    assert_eq!(terminal.class, "DriverShutdown");
}

async fn run_driver_abort_with_accepted_wr(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let server_resources = server_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
        .await
        .unwrap();
    let (server, client) = accept_pair(&listener, &client_engine).await;

    let recv_mr = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let send_mr = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let suppression = server_resources
        .suppress_next_connection_cqe(&server)
        .unwrap();
    let (send_result, returned_send) = client.send(send_mr, None).await;
    send_result.unwrap();
    suppression.wait_observed().await.unwrap();

    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());
    let shutdown_error = server_engine.shutdown().await.unwrap_err();
    assert!(matches!(
        shutdown_error,
        Error::EngineWedged {
            retained_bundles: 1..,
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));
    let (operation_error, returned_recv) = recv.await;
    assert_eq!(
        operation_error.unwrap_err().to_string(),
        shutdown_error.to_string()
    );
    assert!(returned_recv.is_none());
    assert_eq!(
        server.close().await.unwrap_err().to_string(),
        shutdown_error.to_string()
    );
    let diagnostics = server_engine.diagnostics();
    assert_eq!(
        diagnostics.terminal_error.unwrap().message,
        shutdown_error.to_string()
    );
    assert_eq!(diagnostics.registered_quarantined_qps, 1);
    assert_eq!(diagnostics.qp_destroys, 0);
    drop(suppression);

    drop(returned_send);
    let _ = client.close().await;
    let _ = listener.close().await;
    let _ = client_engine.shutdown().await;
    let _ = client_task.await;
    drop(server_resources);
    drop(server);
    drop(client);
    drop(listener);
    drop(server_engine);
    drop(client_engine);
}

async fn run_peer_disconnect(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let recorder = DestructionRecorder::arm(256);
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let client_resources = client_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
        .await
        .unwrap();
    let (server, client) = accept_pair(&listener, &client_engine).await;

    let recv_mr = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let recv_mr_address = recv_mr.inner().as_raw() as usize;
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    client_resources.disconnect_connection(&client).unwrap();
    wait_until(
        "peer disconnect did not enter the local QP ERR close path",
        || {
            let diagnostics = server_engine.diagnostics();
            diagnostics.qp_error_transitions == 1
                && diagnostics.qp_destroys == 1
                && diagnostics.live_connection_reservations == 0
        },
    )
    .await;
    let (recv_result, returned_recv) = recv.await;
    assert!(
        recv_result.is_err(),
        "peer disconnect must fail the outstanding receive"
    );
    drop(returned_recv);
    wait_until(
        "peer disconnect did not complete real MR/QP/CM destruction",
        || {
            let events = recorder.snapshot();
            events.iter().any(|event| {
                event.kind == DestructionKind::MemoryRegion
                    && event.address == recv_mr_address
                    && event.result == Some(0)
            }) && events
                .iter()
                .any(|event| event.kind == DestructionKind::QueuePair)
                && events
                    .iter()
                    .any(|event| event.kind == DestructionKind::CmId)
        },
    )
    .await;
    let disconnect_events = recorder.snapshot();
    assert_cm_ids_were_drained_before_destroy(&disconnect_events);
    let mr = disconnect_events
        .iter()
        .position(|event| {
            event.kind == DestructionKind::MemoryRegion && event.address == recv_mr_address
        })
        .expect("missing peer-disconnect MR destruction");
    assert_eq!(disconnect_events[mr].result, Some(0));
    let qp = disconnect_events
        .iter()
        .position(|event| event.kind == DestructionKind::QueuePair)
        .expect("missing peer-disconnect QP destruction");
    assert!(
        disconnect_events[qp + 1..]
            .iter()
            .any(|event| event.kind == DestructionKind::CmId),
        "missing peer-disconnect CM destruction after QP destruction"
    );
    server.close().await.unwrap();
    let _ = client.close().await;
    listener.close().await.unwrap();
    let (server_shutdown, client_shutdown) =
        tokio::join!(server_engine.shutdown(), client_engine.shutdown());
    server_shutdown.unwrap();
    client_shutdown.unwrap();
    server_task.await.unwrap().unwrap();
    client_task.await.unwrap().unwrap();
    drop(client_resources);
    drop(server);
    drop(client);
    drop(listener);
    drop(server_engine);
    drop(client_engine);

    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_cm_ids_were_drained_before_destroy(&events);
    assert_provider_root_drop_order(&events, mode, 2, true);
}

async fn run_last_engine_handle_drop(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let recorder = DestructionRecorder::arm(64);
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let last_handle = engine.clone();
    let driver_task = tokio::spawn(driver);

    drop(engine);
    tokio::task::yield_now().await;
    assert!(
        !driver_task.is_finished(),
        "dropping a non-final engine clone must not terminate the driver"
    );
    drop(last_handle);
    tokio::time::timeout(Duration::from_secs(5), driver_task)
        .await
        .expect("last engine handle drop did not wake the driver")
        .unwrap()
        .unwrap();

    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_provider_root_drop_order(&events, mode, 1, true);
}

async fn run_injected_driver_failure(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let recorder = DestructionRecorder::arm(64);
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let mr = resources
        .register_memory(32, AccessIntent::LocalOnly)
        .unwrap();
    let mr_address = mr.inner().as_raw() as usize;
    let frontend = engine.clone();
    let driver_task = tokio::spawn(driver);
    tokio::task::yield_now().await;

    let injected = Error::InvalidConfig("injected provider driver failure".into());
    resources.inject_driver_failure(injected.clone()).unwrap();
    let driver_error = tokio::time::timeout(Duration::from_secs(5), driver_task)
        .await
        .expect("injected driver failure did not terminate")
        .unwrap()
        .unwrap_err();
    let shutdown_error = engine.shutdown().await.unwrap_err();
    let connect_error = match frontend.connect("127.0.0.1:9".parse().unwrap()).await {
        Ok(_) => panic!("connect unexpectedly succeeded after injected terminal failure"),
        Err(error) => error,
    };
    let listen_error = match frontend
        .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
        .await
    {
        Ok(_) => panic!("listen unexpectedly succeeded after injected terminal failure"),
        Err(error) => error,
    };
    for observed in [
        &driver_error,
        &shutdown_error,
        &connect_error,
        &listen_error,
    ] {
        assert_eq!(observed.to_string(), injected.to_string());
    }
    let terminal = engine
        .diagnostics()
        .terminal_error
        .expect("injected failure must publish a terminal outcome");
    assert_eq!(terminal.class, "InvalidConfig");
    assert_eq!(terminal.message, injected.to_string());

    drop(mr);
    drop(resources);
    drop(frontend);
    drop(engine);
    let events = recorder.take();
    assert!(!recorder.overflowed());
    let mr_event = &events[position(&events, DestructionKind::MemoryRegion)];
    assert_eq!(mr_event.address, mr_address);
    assert_eq!(mr_event.result, Some(0));
    assert_provider_root_drop_order(&events, mode, 1, false);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn clean_close_records_real_qp_mr_cm_and_canonical_ack_order_in_both_modes() {
    run_clean_close(CompletionMode::Readiness).await;
    run_clean_close(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn held_real_cqe_quarantines_then_recovers_without_rewriting_close_in_both_modes() {
    run_missing_cqe_recovery(CompletionMode::Readiness).await;
    run_missing_cqe_recovery(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn held_real_cqe_wedges_shutdown_and_wakes_all_observers_in_both_modes() {
    run_shutdown_wedge(CompletionMode::Readiness).await;
    run_shutdown_wedge(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn dropping_an_unspawned_driver_is_typed_and_consistent_in_both_modes() {
    run_unspawned_driver_drop(CompletionMode::Readiness).await;
    run_unspawned_driver_drop(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn aborting_the_driver_with_an_accepted_wr_wedges_and_wakes_in_both_modes() {
    run_driver_abort_with_accepted_wr(CompletionMode::Readiness).await;
    run_driver_abort_with_accepted_wr(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn peer_disconnect_uses_the_same_explicit_local_qp_err_close_path_in_both_modes() {
    run_peer_disconnect(CompletionMode::Readiness).await;
    run_peer_disconnect(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn last_engine_clone_drop_terminates_driver_and_releases_resources_in_both_modes() {
    run_last_engine_handle_drop(CompletionMode::Readiness).await;
    run_last_engine_handle_drop(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn injected_failure_is_exact_and_records_real_destruction_in_both_modes() {
    run_injected_driver_failure(CompletionMode::Readiness).await;
    run_injected_driver_failure(CompletionMode::Polling).await;
}
