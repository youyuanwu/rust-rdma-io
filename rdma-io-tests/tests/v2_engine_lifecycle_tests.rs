use std::task::Poll;
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::test_support::{DestructionEvent, DestructionKind, DestructionRecorder};
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, RdmaConnection, RdmaEngine, RdmaEngineBuilder,
    RdmaListener, RdmaListenerConfig,
};
use rdma_io::wc::WcOpcode;
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
    assert_eq!(server_engine.diagnostics().quarantined_bundles, 0);
    assert_eq!(client_engine.diagnostics().quarantined_bundles, 0);
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

async fn run_missing_flush_cqe_qp_destroy_fallback(mode: CompletionMode) {
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
    let (server_b, client_b) = accept_pair(&listener, &client_engine).await;
    let old_connection_identity = server_resources
        .connection_registry_identity(&server)
        .unwrap();
    let old_qp_num = server.identity().qp_num();
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let accepted = server_resources.accepted_operation_wr_ids(&server).unwrap();
    assert_eq!(accepted.len(), 1);
    let old_wr_id = accepted[0];
    let old_operation_identity = server_resources
        .operation_registry_identity(old_wr_id)
        .unwrap();
    let suppression = server_resources
        .suppress_next_connection_flush_cqe(&server)
        .unwrap();
    let recorder = DestructionRecorder::arm(256);

    tokio::time::timeout(Duration::from_secs(3), server.close())
        .await
        .expect("connection close did not reach its configured deadline")
        .unwrap();
    let (operation_result, returned_recv) = recv.await;
    assert!(matches!(operation_result, Err(Error::TransportClosed)));
    assert!(returned_recv.is_none());

    let closed = server_engine.diagnostics();
    assert_eq!(closed.accepted_outstanding_operations, 0);
    assert_eq!(closed.registered_operations, 0);
    assert_eq!(closed.free_cq_credits, 64);
    assert_eq!(closed.quarantined_bundles, 0);
    assert_eq!(closed.quarantined_operations, 0);
    assert_eq!(closed.retained_cq_credits, 0);
    assert_eq!(closed.live_connection_reservations, 1);
    assert_eq!(closed.qp_destroys, 1);
    assert_eq!(closed.operations_released_after_qp_destroy, 1);
    assert!(closed.connections_drain_started >= 1);
    assert!(closed.qp_error_transitions >= 1);
    let after_close = recorder.snapshot();
    let qp_destroy = position(&after_close, DestructionKind::QueuePair);
    let mr_release = position(&after_close, DestructionKind::MemoryRegion);
    assert!(
        qp_destroy < mr_release,
        "the owning QP must be synchronously destroyed before its unresolved MR is released"
    );

    drop(suppression);
    let recv_b_mr = server_b
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let mut recv_b = Box::pin(server_b.recv(recv_b_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv_b.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let accepted_b = server_resources
        .accepted_operation_wr_ids(&server_b)
        .unwrap();
    assert_eq!(accepted_b.len(), 1);
    let new_operation_identity = server_resources
        .operation_registry_identity(accepted_b[0])
        .unwrap();
    assert_eq!(new_operation_identity.0, old_operation_identity.0);
    assert_ne!(new_operation_identity.1, old_operation_identity.1);

    let rejected_before = server_resources.instrumentation().unwrap().cqes_rejected;
    server_resources
        .inject_completion(old_wr_id, old_qp_num, WcOpcode::Recv)
        .unwrap();
    assert_eq!(
        server_resources.instrumentation().unwrap().cqes_rejected,
        rejected_before + 1
    );
    futures_util::future::poll_fn(|cx| {
        assert!(recv_b.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;

    let mut send_b_mr = client_b
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    send_b_mr.as_mut_slice()[0] = 73;
    let ((recv_b_result, returned_recv_b), (send_b_result, returned_send_b)) =
        tokio::join!(recv_b, client_b.send(send_b_mr, None));
    recv_b_result.unwrap();
    send_b_result.unwrap();
    assert_eq!(returned_recv_b.unwrap().as_slice()[0], 73);
    drop(returned_send_b);

    client.close().await.unwrap();
    let (server_c, client_c) = accept_pair(&listener, &client_engine).await;
    let new_connection_identity = server_resources
        .connection_registry_identity(&server_c)
        .unwrap();
    assert_eq!(new_connection_identity.0, old_connection_identity.0);
    assert_ne!(new_connection_identity.1, old_connection_identity.1);

    let (server_c_close, client_c_close) = tokio::join!(server_c.close(), client_c.close());
    server_c_close.unwrap();
    client_c_close.unwrap();
    let (server_b_close, client_b_close) = tokio::join!(server_b.close(), client_b.close());
    server_b_close.unwrap();
    client_b_close.unwrap();
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
    drop(server_b);
    drop(client_b);
    drop(server_c);
    drop(client_c);
    drop(listener);
    drop(server_engine);
    drop(client_engine);

    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_cm_ids_were_drained_before_destroy(&events);
}

async fn run_shutdown_qp_destroy_fallback(mode: CompletionMode) {
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

    tokio::time::timeout(Duration::from_secs(3), server_engine.shutdown())
        .await
        .expect("engine shutdown did not reach its configured close deadline")
        .unwrap();
    server_task.await.unwrap().unwrap();
    let (operation_error, returned_recv) = recv.await;
    assert!(matches!(operation_error, Err(Error::TransportClosed)));
    assert!(returned_recv.is_none());
    let diagnostics = server_engine.diagnostics();
    assert!(diagnostics.terminal_error.is_none());
    assert_eq!(diagnostics.accepted_outstanding_operations, 0);
    assert_eq!(diagnostics.registered_operations, 0);
    assert_eq!(diagnostics.quarantined_bundles, 0);
    assert_eq!(diagnostics.qp_destroys, 1);
    assert_eq!(diagnostics.operations_released_after_qp_destroy, 1);
    assert_eq!(diagnostics.engine_wedges, 0);
    assert!(diagnostics.connections_drain_started >= 1);
    assert!(diagnostics.qp_error_transitions >= 1);
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

async fn run_qp_destroy_failure_quarantine(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
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
    let server_resources = server_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen("0.0.0.0:0".parse().unwrap(), RdmaListenerConfig::default())
        .await
        .unwrap();
    let (server, client) = accept_pair(&listener, &client_engine).await;
    let recv_mr = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut recv = Box::pin(server.recv(recv_mr, None));
    futures_util::future::poll_fn(|cx| {
        assert!(recv.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let suppression = server_resources
        .suppress_next_connection_flush_cqe(&server)
        .unwrap();
    server_resources
        .fail_next_connection_qp_destroy(&server)
        .unwrap();
    let recorder = DestructionRecorder::arm(256);

    let close_error = tokio::time::timeout(Duration::from_secs(3), server.close())
        .await
        .expect("destroy failure did not publish connection quarantine")
        .unwrap_err();
    assert!(matches!(
        close_error,
        Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));
    let (operation_error, returned_recv) = recv.await;
    assert!(matches!(operation_error, Err(Error::TransportClosed)));
    assert!(returned_recv.is_none());

    let diagnostics = server_engine.diagnostics();
    assert_eq!(diagnostics.qp_destroys, 0);
    assert_eq!(diagnostics.operations_released_after_qp_destroy, 0);
    assert_eq!(diagnostics.accepted_outstanding_operations, 1);
    assert_eq!(diagnostics.registered_operations, 1);
    assert_eq!(diagnostics.free_cq_credits, 63);
    assert_eq!(diagnostics.retained_cq_credits, 1);
    assert_eq!(diagnostics.quarantined_operations, 1);
    assert_eq!(diagnostics.quarantined_mrs, 1);
    assert_eq!(diagnostics.quarantined_bundles, 1);
    let events = recorder.snapshot();
    assert!(!recorder.overflowed());
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == DestructionKind::QueuePair
                    && event.result.is_some_and(|result| result != 0)
            })
            .count(),
        1,
        "exactly one injected result-aware QP destruction must fail"
    );
    assert!(events.iter().any(|event| {
        event.kind == DestructionKind::QueuePair && event.result.is_some_and(|result| result != 0)
    }));
    assert!(
        events
            .iter()
            .all(|event| event.kind != DestructionKind::MemoryRegion),
        "QP destroy failure must retain the posted MR"
    );
    assert!(
        events
            .iter()
            .all(|event| event.kind != DestructionKind::CompletionQueue),
        "the externally supplied shared CQ must remain owned exactly once"
    );

    drop(suppression);
    let shutdown_error = server_engine.shutdown().await.unwrap_err();
    assert!(matches!(
        shutdown_error,
        Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 1,
            cq_debt: 1
        }
    ));
    assert!(matches!(
        server_task.await.unwrap(),
        Err(Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));

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
    let final_events = recorder.take();
    assert!(
        final_events
            .iter()
            .all(|event| event.kind != DestructionKind::MemoryRegion),
        "terminal quarantine must keep the failed QP's MR registered"
    );
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
    assert_eq!(diagnostics.quarantined_bundles, 1);
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

async fn exchange_byte(sender: &RdmaConnection, receiver: &RdmaConnection, value: u8) {
    let recv = receiver
        .register_memory(8, AccessIntent::LocalOnly)
        .unwrap();
    let mut send = sender.register_memory(8, AccessIntent::LocalOnly).unwrap();
    send.as_mut_slice()[0] = value;
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(receiver.recv(recv, None), sender.send(send, None));
    recv_result.unwrap();
    send_result.unwrap();
    assert_eq!(recv.unwrap().as_slice()[0], value);
    drop(send);
}

async fn run_clean_retirement_destroy_quarantine(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .connection_drain_deadline(Duration::from_millis(100))
        .shutdown_deadline(Duration::from_millis(300))
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .connection_drain_deadline(Duration::from_millis(100))
        .shutdown_deadline(Duration::from_millis(300))
        .build()
        .unwrap();
    let server_resources = server_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(4),
        )
        .await
        .unwrap();
    let (failed_server, failed_client) = accept_pair(&listener, &client_engine).await;
    let (healthy_server, healthy_client) = accept_pair(&listener, &client_engine).await;
    server_resources
        .fail_next_connection_qp_destroy(&failed_server)
        .unwrap();
    let recorder = DestructionRecorder::arm(256);

    let close_error = tokio::time::timeout(Duration::from_secs(5), failed_server.close())
        .await
        .expect("clean retirement destroy failure did not publish")
        .unwrap_err();
    let Error::ConnectionDestroyQuarantined { cause } = close_error else {
        panic!("unexpected clean retirement outcome: {close_error}");
    };
    assert!(cause.contains("Device or resource busy") || cause.contains("os error 16"));
    let repeated = failed_server.close().await.unwrap_err();
    assert!(matches!(
        repeated,
        Error::ConnectionDestroyQuarantined {
            cause: ref repeated_cause
        } if repeated_cause == &cause
    ));

    let diagnostics = server_engine.diagnostics();
    assert!(diagnostics.terminal_error.is_none());
    assert_eq!(diagnostics.quarantined_bundles, 1);
    assert_eq!(diagnostics.live_connection_reservations, 2);
    assert_eq!(diagnostics.draining_connection_reservations, 1);
    assert_eq!(diagnostics.registered_live_qps, 1);
    assert_eq!(diagnostics.free_connection_slots, 2);
    assert_eq!(diagnostics.connections_quarantined, 1);
    assert_eq!(diagnostics.connection_quarantine_outcomes, 1);
    assert_eq!(diagnostics.qp_destroys, 0);
    assert!(!server_task.is_finished());

    exchange_byte(&healthy_client, &healthy_server, 0x5a).await;
    assert!(!server_task.is_finished());
    assert!(server_engine.diagnostics().terminal_error.is_none());

    let events = recorder.snapshot();
    assert!(!recorder.overflowed());
    let failed_qps = events
        .iter()
        .filter(|event| {
            event.kind == DestructionKind::QueuePair
                && event.result.is_some_and(|result| result != 0)
        })
        .collect::<Vec<_>>();
    assert_eq!(failed_qps.len(), 1);
    assert!(!events.iter().any(|event| {
        event.kind == DestructionKind::QueuePair
            && event.address == failed_qps[0].address
            && event.result == Some(0)
    }));

    healthy_server.close().await.unwrap();
    healthy_client.close().await.unwrap();
    drop(failed_client);
    listener.close().await.unwrap();
    assert!(matches!(
        server_engine.shutdown().await,
        Err(Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 0,
            cq_debt: 0
        })
    ));
    assert!(matches!(
        server_task.await.unwrap(),
        Err(Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 0,
            cq_debt: 0
        })
    ));
    client_engine.shutdown().await.unwrap();
    client_task.await.unwrap().unwrap();
}

async fn run_setup_rollback_destroy_quarantines(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .connection_drain_deadline(Duration::from_millis(100))
        .shutdown_deadline(Duration::from_millis(300))
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .connection_drain_deadline(Duration::from_millis(100))
        .shutdown_deadline(Duration::from_millis(300))
        .build()
        .unwrap();
    let server_resources = server_engine.test_resources().unwrap();
    let client_resources = client_engine.test_resources().unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(4),
        )
        .await
        .unwrap();
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let recorder = DestructionRecorder::arm(256);

    let outbound_original = "injected outbound installation failure";
    client_resources
        .fail_next_setup_rollback_qp_destroy(Error::InvalidConfig(outbound_original.into()))
        .unwrap();
    assert_eq!(recorder.snapshot().len(), 0);
    let duplicate_error = client_resources
        .fail_next_setup_rollback_qp_destroy(Error::InvalidConfig(
            "rejected duplicate setup rollback failure".into(),
        ))
        .unwrap_err();
    assert!(matches!(
        duplicate_error,
        Error::InvalidConfig(ref message)
            if message == "a setup rollback failure is already pending"
    ));
    assert_eq!(
        recorder.snapshot().len(),
        0,
        "rejecting a duplicate setup rollback injection must not register and drop an MR"
    );
    let outbound_error = match client_engine.connect(address).await {
        Ok(_) => panic!("outbound setup rollback unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(matches!(
        outbound_error,
        Error::InvalidConfig(ref message) if message == outbound_original
    ));
    wait_until("outbound rollback quarantine was not published", || {
        let diagnostics = client_engine.diagnostics();
        diagnostics.quarantined_bundles == 1
            && diagnostics.oldest_quarantine_age.is_some()
            && diagnostics.live_connection_reservations == 1
            && diagnostics.establishing_connection_reservations == 0
            && diagnostics.free_connection_slots == 3
            && diagnostics.connections_quarantined == 1
            && diagnostics.connection_quarantine_outcomes == 1
    })
    .await;
    assert!(!client_task.is_finished());
    assert!(client_engine.diagnostics().terminal_error.is_none());

    let inbound_original = "injected inbound installation failure";
    server_resources
        .fail_next_setup_rollback_qp_destroy(Error::InvalidConfig(inbound_original.into()))
        .unwrap();
    let failed_client_task = tokio::spawn({
        let client_engine = client_engine.clone();
        async move { client_engine.connect(address).await }
    });
    let inbound_error = match tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("inbound rollback failure did not complete accept")
    {
        Ok(_) => panic!("inbound setup rollback unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(matches!(
        inbound_error,
        Error::InvalidConfig(ref message) if message == inbound_original
    ));
    let peer_result = tokio::time::timeout(Duration::from_secs(3), failed_client_task)
        .await
        .expect("inbound rollback did not reject the peer promptly")
        .expect("failed peer connect task panicked");
    let peer_error = match peer_result {
        Ok(_) => panic!("inbound rollback peer unexpectedly connected"),
        Err(error) => error,
    };
    assert!(
        matches!(&peer_error, Error::Verbs(_)) && peer_error.to_string().contains("Rejected"),
        "unexpected inbound rollback peer outcome: {peer_error}"
    );
    wait_until("inbound rollback quarantine was not published", || {
        let diagnostics = server_engine.diagnostics();
        diagnostics.quarantined_bundles == 1
            && diagnostics.oldest_quarantine_age.is_some()
            && diagnostics.live_connection_reservations == 1
            && diagnostics.establishing_connection_reservations == 0
            && diagnostics.free_connection_slots == 3
            && diagnostics.connections_quarantined == 1
            && diagnostics.connection_quarantine_outcomes == 1
            && diagnostics.inbound_requests_rejected == 1
            && diagnostics.inbound_rejected_setup_failure == 1
    })
    .await;
    assert!(!server_task.is_finished());
    assert!(server_engine.diagnostics().terminal_error.is_none());

    let rollback_events = recorder.snapshot();
    assert!(!recorder.overflowed());
    assert_eq!(
        rollback_events
            .iter()
            .filter(|event| {
                event.kind == DestructionKind::QueuePair
                    && event.result.is_some_and(|result| result != 0)
            })
            .count(),
        2
    );
    assert!(
        rollback_events
            .iter()
            .all(|event| event.kind != DestructionKind::MemoryRegion),
        "setup rollback quarantine released one of its two registered retained MRs before the failed QP destruction boundary"
    );

    let (healthy_server, healthy_client) = accept_pair(&listener, &client_engine).await;
    exchange_byte(&healthy_client, &healthy_server, 0xa5).await;
    wait_until(
        "rejected peer reservation was not retired before diagnostics",
        || {
            let diagnostics = client_engine.diagnostics();
            diagnostics.free_connection_slots == 2 && diagnostics.live_connection_reservations == 2
        },
    )
    .await;
    let server_diagnostics = server_engine.diagnostics();
    assert!(server_diagnostics.terminal_error.is_none());
    assert_eq!(server_diagnostics.quarantined_bundles, 1);
    assert_eq!(server_diagnostics.free_connection_slots, 2);
    assert_eq!(server_diagnostics.live_connection_reservations, 2);
    assert_eq!(server_diagnostics.retired_connection_slots, 0);
    assert_eq!(server_diagnostics.connections_quarantined, 1);
    assert_eq!(server_diagnostics.connection_quarantine_outcomes, 1);
    assert_eq!(server_diagnostics.inbound_requests_rejected, 1);
    assert_eq!(server_diagnostics.inbound_rejected_setup_failure, 1);
    assert!(!server_task.is_finished());
    let client_diagnostics = client_engine.diagnostics();
    assert!(client_diagnostics.terminal_error.is_none());
    assert_eq!(client_diagnostics.quarantined_bundles, 1);
    assert_eq!(client_diagnostics.free_connection_slots, 2);
    assert_eq!(client_diagnostics.live_connection_reservations, 2);
    assert_eq!(client_diagnostics.retired_connection_slots, 0);
    assert_eq!(client_diagnostics.connections_quarantined, 1);
    assert_eq!(client_diagnostics.connection_quarantine_outcomes, 1);
    assert!(!client_task.is_finished());

    let events = recorder.snapshot();
    assert!(!recorder.overflowed());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        2,
        "only the healthy exchange MRs may be deregistered while setup rollback MRs remain retained"
    );
    let failed_qps = events
        .iter()
        .filter(|event| {
            event.kind == DestructionKind::QueuePair
                && event.result.is_some_and(|result| result != 0)
        })
        .collect::<Vec<_>>();
    assert_eq!(failed_qps.len(), 2);
    for failed in failed_qps {
        assert!(!events.iter().any(|event| {
            event.kind == DestructionKind::QueuePair
                && event.address == failed.address
                && event.result == Some(0)
        }));
    }

    healthy_server.close().await.unwrap();
    healthy_client.close().await.unwrap();
    listener.close().await.unwrap();
    let server_shutdown = server_engine.shutdown().await;
    assert!(
        matches!(
            server_shutdown,
            Err(Error::EngineWedged {
                retained_bundles: 1,
                outstanding_operations: 0,
                cq_debt: 0
            })
        ),
        "unexpected server shutdown outcome: {server_shutdown:?}"
    );
    assert!(matches!(
        server_task.await.unwrap(),
        Err(Error::EngineWedged {
            retained_bundles: 1,
            outstanding_operations: 0,
            cq_debt: 0
        })
    ));
    client_task.abort();
    let _ = client_task.await;
    drop(client_engine);
    let final_events = recorder.snapshot();
    assert!(!recorder.overflowed());
    assert_eq!(
        final_events
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        2,
        "terminal setup rollback quarantine released one of its registered retained MRs"
    );
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
            events
                .iter()
                .any(|event| event.kind == DestructionKind::MemoryRegion && event.result == Some(0))
                && events
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
        .position(|event| event.kind == DestructionKind::MemoryRegion)
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
    assert_eq!(mr_event.result, Some(0));
    assert_provider_root_drop_order(&events, mode, 1, false);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn clean_close_records_real_qp_mr_cm_and_canonical_ack_order_in_both_modes() {
    run_clean_close(CompletionMode::Readiness).await;
    run_clean_close(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn omitted_flush_cqe_uses_qp_destroy_before_clean_reclaim_in_both_modes() {
    run_missing_flush_cqe_qp_destroy_fallback(CompletionMode::Readiness).await;
    run_missing_flush_cqe_qp_destroy_fallback(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn held_real_cqe_uses_qp_destroy_for_clean_shutdown_in_both_modes() {
    run_shutdown_qp_destroy_fallback(CompletionMode::Readiness).await;
    run_shutdown_qp_destroy_fallback(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn result_aware_qp_destroy_failure_quarantines_mr_and_debt_in_both_modes() {
    run_qp_destroy_failure_quarantine(CompletionMode::Readiness).await;
    run_qp_destroy_failure_quarantine(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn clean_retirement_destroy_failure_is_connection_local_in_both_modes() {
    run_clean_retirement_destroy_quarantine(CompletionMode::Readiness).await;
    run_clean_retirement_destroy_quarantine(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn setup_rollback_destroy_failures_preserve_primary_errors_in_both_modes() {
    run_setup_rollback_destroy_quarantines(CompletionMode::Readiness).await;
    run_setup_rollback_destroy_quarantines(CompletionMode::Polling).await;
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
