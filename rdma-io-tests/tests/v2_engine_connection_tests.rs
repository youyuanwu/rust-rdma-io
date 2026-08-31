use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

use rdma_io::cm::{ConnParam, RdmaCmDeviceList};
use rdma_io::test_support::destruction::{DestructionKind, DestructionRecorder};
use rdma_io::test_support::engine_driver::TestEngineResources;
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, RdmaConnection, RdmaConnectionConfig, RdmaEngine,
    RdmaEngineBuilder,
};
use rdma_io_tests::test_helpers::{bind_listener_with_retry, connect_addr_for, has_software_rdma};

fn software_device_name() -> Option<String> {
    let list = RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn conn_param(config: &RdmaConnectionConfig) -> ConnParam {
    ConnParam {
        responder_resources: config.responder_resource_count() as u8,
        initiator_depth: config.initiator_depth_count() as u8,
        retry_count: config.retry_count_value() as u8,
        rnr_retry_count: config.rnr_retry_count_value() as u8,
    }
}

async fn establish_pair(
    engine: &RdmaEngine,
    resources: &TestEngineResources,
    config: RdmaConnectionConfig,
    use_default_client: bool,
) -> (RdmaConnection, RdmaConnection) {
    let listener = bind_listener_with_retry().await;
    let address = connect_addr_for(listener.local_addr());
    let server_config = config.clone();
    let server_resources = resources.clone();
    let server = async move {
        let cm_id = listener.get_request().await.unwrap();
        server_resources.require_context(&cm_id).unwrap();
        let qp = server_resources
            .create_qp(
                &cm_id,
                server_config.maximum_send_work_requests() as u32,
                server_config.maximum_receive_work_requests() as u32,
            )
            .unwrap();
        cm_id.accept(&conn_param(&server_config)).unwrap();
        listener.await_established().await.unwrap();
        let cm = rdma_io::async_cm::AsyncCmListener::migrate_accepted(cm_id).unwrap();
        server_resources
            .install_connection(qp, cm, server_config)
            .unwrap()
    };
    let client = async {
        if use_default_client {
            engine.connect(address).await
        } else {
            engine.connect_with_config(address, config).await
        }
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server, client) = tokio::join!(server, client);
        (server, client.unwrap())
    })
    .await
    .expect("engine CM establishment timed out")
}

async fn exercise_operations(server: &RdmaConnection, client: &RdmaConnection) {
    let recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut send = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    send.as_mut_slice()[..12].copy_from_slice(b"engine-send!");
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(server.recv(recv, None), client.send(send, None));
    recv_result.unwrap();
    send_result.unwrap();
    assert_eq!(&recv.unwrap().as_slice()[..12], b"engine-send!");
    assert!(send.is_some());

    let remote_write = server
        .register_memory(64, AccessIntent::RemoteReadWrite)
        .unwrap();
    let mut local_write = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    local_write.as_mut_slice()[..12].copy_from_slice(b"engine-write");
    let (write_result, local_write) = client
        .write(local_write, remote_write.to_remote(), Some((0, 12)))
        .await;
    write_result.unwrap();
    assert!(local_write.is_some());
    assert_eq!(&remote_write.as_slice()[..12], b"engine-write");

    let mut remote_read = server
        .register_memory(64, AccessIntent::RemoteReadWrite)
        .unwrap();
    remote_read.as_mut_slice()[..11].copy_from_slice(b"engine-read");
    let local_read = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let (read_result, local_read) = client
        .read(local_read, remote_read.to_remote(), Some((0, 11)))
        .await;
    read_result.unwrap();
    assert_eq!(&local_read.unwrap().as_slice()[..11], b"engine-read");
}

async fn run_mode(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);

    let (default_server, default_client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;
    let configured = RdmaConnectionConfig::default()
        .max_send_wr(8)
        .max_recv_wr(8)
        .responder_resources(0)
        .initiator_depth(0);
    let (configured_server, configured_client) =
        establish_pair(&engine, &resources, configured, false).await;

    assert_eq!(
        default_client.peer_addr().unwrap(),
        default_server.local_addr().unwrap()
    );
    assert_eq!(
        configured_client.peer_addr().unwrap(),
        configured_server.local_addr().unwrap()
    );
    exercise_operations(&default_server, &default_client).await;

    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.shared_contexts, 1);
    assert_eq!(diagnostics.shared_protection_domains, 1);
    assert_eq!(diagnostics.shared_completion_queues, 1);
    assert_eq!(diagnostics.shared_cm_event_channels, 1);
    assert_eq!(
        diagnostics.shared_completion_channels,
        usize::from(mode == CompletionMode::Readiness)
    );
    assert_eq!(diagnostics.live_connection_reservations, 4);
    assert_eq!(diagnostics.connections_opened, 2);
    assert!(diagnostics.cm_events_processed >= 6);
    assert_eq!(diagnostics.cm_events_rejected, 0);
    assert_eq!(diagnostics.operations_accepted, 4);
    assert_eq!(diagnostics.operations_completed, 4);

    resources.disconnect_connection(&default_server).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.diagnostics().cm_events_processed > diagnostics.cm_events_processed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect event was not routed by the shared CM driver");
    let disconnected_mr = default_client
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let (disconnected, returned) = default_client.send(disconnected_mr, None).await;
    assert!(matches!(disconnected, Err(Error::TransportClosed)));
    assert!(returned.is_some());

    let configured_recv = configured_server
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let configured_send = configured_client
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let ((recv_result, recv), (send_result, send)) = tokio::join!(
        configured_server.recv(configured_recv, None),
        configured_client.send(configured_send, None),
    );
    recv_result.unwrap();
    send_result.unwrap();
    assert!(recv.is_some());
    assert!(send.is_some());

    for connection in [
        &default_server,
        &default_client,
        &configured_server,
        &configured_client,
    ] {
        connection.close().await.unwrap();
    }
    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
    drop(default_server);
    drop(default_client);
    drop(configured_server);
    drop(configured_client);
    drop(resources);
    drop(engine);
}

async fn run_rejected_connect(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);
    let (server, client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;

    let reject_listener = bind_listener_with_retry().await;
    let reject_address = connect_addr_for(reject_listener.local_addr());
    let reject_server = async move {
        let cm_id = reject_listener.get_request().await.unwrap();
        cm_id.reject(&[]).unwrap();
    };
    let rejected = tokio::time::timeout(Duration::from_secs(10), async {
        let ((), result) = tokio::join!(reject_server, engine.connect(reject_address));
        result
    })
    .await
    .expect("rejected outbound connection timed out");
    assert!(rejected.is_err());
    assert_eq!(engine.diagnostics().live_connection_reservations, 2);
    assert_eq!(engine.diagnostics().connections_failed, 1);

    let recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let send = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(server.recv(recv, None), client.send(send, None));
    recv_result.unwrap();
    send_result.unwrap();
    assert!(recv.is_some());
    assert!(send.is_some());

    server.close().await.unwrap();
    client.close().await.unwrap();
    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn outbound_connect_and_operations_use_one_shared_engine_in_both_modes() {
    run_mode(CompletionMode::Readiness).await;
    run_mode(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn rejected_cm_event_fails_only_its_request_in_both_modes() {
    run_rejected_connect(CompletionMode::Readiness).await;
    run_rejected_connect(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn withholding_the_driver_prevents_cm_progress_and_cancellation_releases_admission() {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Readiness)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let listener = bind_listener_with_retry().await;
    let address = connect_addr_for(listener.local_addr());
    let mut connect = Box::pin(engine.connect(address));
    poll_fn(|cx| {
        assert!(connect.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(engine.diagnostics().live_connection_reservations, 1);
    drop(connect);

    let driver_task = tokio::spawn(driver);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.diagnostics().live_connection_reservations == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled connect reservation was not released");

    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn aggregate_outbound_admission_rejects_before_a_second_cm_request() {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);
    let (server, client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;
    let error = match engine.connect(client.peer_addr().unwrap()).await {
        Ok(_) => panic!("the aggregate connection reservation must be exhausted"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::CapacityExhausted));
    assert_eq!(engine.diagnostics().connection_capacity_exhausted, 1);
    server.close().await.unwrap();
    client.close().await.unwrap();
    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn cancellation_after_rdma_connect_waits_for_the_routed_terminal_event() {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Readiness)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let driver_task = tokio::spawn(driver);
    let listener = bind_listener_with_retry().await;
    let address = connect_addr_for(listener.local_addr());
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (reject_tx, reject_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let cm_id = listener.get_request().await.unwrap();
        request_seen_tx.send(()).unwrap();
        reject_rx.await.unwrap();
        cm_id.reject(&[]).unwrap();
    });
    let client_engine = engine.clone();
    let connect_task = tokio::spawn(async move { client_engine.connect(address).await });
    tokio::time::timeout(Duration::from_secs(10), request_seen_rx)
        .await
        .expect("server did not receive the engine's rdma_connect request")
        .unwrap();
    connect_task.abort();
    let join_error = match connect_task.await {
        Ok(_) => panic!("cancelled connect task unexpectedly completed"),
        Err(error) => error,
    };
    assert!(join_error.is_cancelled());
    reject_tx.send(()).unwrap();
    server.await.unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let diagnostics = engine.diagnostics();
            if diagnostics.live_connection_reservations == 0
                && diagnostics.free_connection_slots == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled post-connect route did not clean up after rejection");
    assert_eq!(engine.diagnostics().connections_opened, 0);

    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn established_close_releases_resources_and_reuses_aggregate_capacity() {
    if !has_software_rdma() {
        return;
    }
    let recorder = DestructionRecorder::arm(128);
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);

    let (first_server, first_client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;
    assert_eq!(engine.diagnostics().live_connection_reservations, 2);
    assert_eq!(engine.diagnostics().free_connection_slots, 0);

    let first_client_close_one = first_client.clone();
    let first_client_close_two = first_client.clone();
    drop(first_client);
    let (server_close, client_close_one, client_close_two) = tokio::join!(
        first_server.close(),
        first_client_close_one.close(),
        first_client_close_two.close(),
    );
    server_close.unwrap();
    client_close_one.unwrap();
    client_close_two.unwrap();
    drop(first_server);
    drop(first_client_close_one);
    drop(first_client_close_two);
    assert_eq!(engine.diagnostics().live_connection_reservations, 0);
    assert_eq!(engine.diagnostics().free_connection_slots, 2);

    let (second_server, second_client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;
    assert_eq!(engine.diagnostics().live_connection_reservations, 2);
    assert_eq!(engine.diagnostics().free_connection_slots, 0);
    let second_client_duplicate = second_client.clone();
    let (server_close, client_close, duplicate_close) = tokio::join!(
        second_server.close(),
        second_client.close(),
        second_client_duplicate.close(),
    );
    server_close.unwrap();
    client_close.unwrap();
    duplicate_close.unwrap();
    assert_eq!(engine.diagnostics().live_connection_reservations, 0);
    assert_eq!(engine.diagnostics().free_connection_slots, 2);
    drop(second_server);
    drop(second_client);
    drop(second_client_duplicate);

    driver_task.abort();
    assert!(driver_task.await.unwrap_err().is_cancelled());
    let shutdown = engine.shutdown().await.unwrap_err();
    assert!(matches!(shutdown, Error::DriverShutdown));

    drop(resources);
    drop(engine);
    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::QueuePair)
            .count(),
        4
    );
    assert!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::CmId)
            .count()
            >= 4
    );
    for kind in [
        DestructionKind::ProtectionDomain,
        DestructionKind::CompletionQueue,
        DestructionKind::CmEventChannel,
        DestructionKind::ContextFacade,
        DestructionKind::RdmaFreeDevices,
    ] {
        assert!(
            events.iter().any(|event| event.kind == kind),
            "missing destruction evidence for {kind:?}"
        );
    }
    assert!(
        !events
            .iter()
            .any(|event| event.kind == DestructionKind::IbvCloseDevice)
    );
    assert_eq!(
        events.last().map(|event| event.kind),
        Some(DestructionKind::RdmaFreeDevices)
    );
}

#[test]
fn outbound_api_surface_has_exact_future_outputs() {
    fn assert_send<T: Send>(_: T) {}
    fn check(engine: &RdmaEngine, address: std::net::SocketAddr) {
        assert_send(engine.connect(address));
        assert_send(engine.connect_with_config(address, RdmaConnectionConfig::default()));
    }
    let _ = check;
}
