use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use rdma_io::cm::{CmEventType, ConnParam, RdmaCmDeviceList};
use rdma_io::v2::test_support::{DestructionKind, DestructionRecorder, TestEngineResources};
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, RdmaConnection, RdmaConnectionConfig, RdmaEngine,
    RdmaEngineBuilder, RdmaEngineLifecycle,
};
use rdma_io_tests::test_helpers::{bind_listener_with_retry, connect_addr_for, has_software_rdma};

fn software_device_name() -> Option<String> {
    let list = RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    future.poll(&mut context)
}

async fn wait_until(
    timeout: Duration,
    description: &'static str,
    mut condition: impl FnMut() -> bool,
) {
    tokio::time::timeout(timeout, async {
        loop {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"));
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
    let (max_send_wr, max_recv_wr, parameter) = if use_default_client {
        (19, 34, ConnParam::default())
    } else {
        (
            8,
            8,
            ConnParam {
                responder_resources: 0,
                initiator_depth: 0,
                retry_count: 7,
                rnr_retry_count: 7,
            },
        )
    };
    let server_resources = resources.clone();
    let server = async move {
        let cm_id = listener.get_request().await.unwrap();
        server_resources.require_context(&cm_id).unwrap();
        let qp = server_resources
            .create_qp(&cm_id, max_send_wr, max_recv_wr)
            .unwrap();
        cm_id.accept(&parameter).unwrap();
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
    assert_eq!(diagnostics.live_connections, 4);
    assert_eq!(diagnostics.registered_operations, 0);
    assert_eq!(diagnostics.accepted_operations, 0);

    resources.disconnect_connection(&default_server).unwrap();
    let disconnected_mr = default_client
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    let (disconnected, returned) = default_client.send(disconnected_mr, None).await;
    assert!(matches!(disconnected, Err(Error::TransportClosed)));
    drop(returned);
    default_server.close().await.unwrap();
    default_client.close().await.unwrap();
    assert_eq!(engine.diagnostics().live_connections, 2);

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

    for connection in [&configured_server, &configured_client] {
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
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let diagnostics = engine.diagnostics();
            if diagnostics.live_connections == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejected connection resources were not retired");
    assert_eq!(engine.diagnostics().live_connections, 2);

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

async fn run_over_budget_failed_connects(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    const CM_BUDGET: usize = 2;
    const ATTEMPTS: usize = CM_BUDGET + 3;

    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(ATTEMPTS)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .cm_event_budget(CM_BUDGET)
        .build()
        .unwrap();
    let driver_task = tokio::spawn(driver);
    let listener = bind_listener_with_retry().await;
    let address = connect_addr_for(listener.local_addr());
    let server_task = tokio::spawn(async move {
        for _ in 0..ATTEMPTS {
            let cm_id = listener.get_request().await.unwrap();
            cm_id.reject(&[]).unwrap();
        }
    });

    let mut connects = tokio::task::JoinSet::new();
    for _ in 0..ATTEMPTS {
        let engine = engine.clone();
        connects.spawn(async move { engine.connect(address).await });
    }
    let failures = tokio::time::timeout(Duration::from_secs(20), async {
        let mut failures = 0;
        while let Some(result) = connects.join_next().await {
            assert!(result.unwrap().is_err());
            failures += 1;
        }
        failures
    })
    .await
    .expect("over-budget failed connects stopped cooperative CM progress");
    assert_eq!(failures, ATTEMPTS);
    server_task.await.unwrap();

    wait_until(
        Duration::from_secs(10),
        "over-budget failed connect resources were not retired",
        || engine.diagnostics().live_connections == 0,
    )
    .await;

    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}

async fn run_shutdown_with_undelivered_connect(mode: CompletionMode) {
    struct WakeCounter(AtomicUsize);
    impl futures_util::task::ArcWake for WakeCounter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    let recorder = DestructionRecorder::arm(128);
    let (engine, driver) = RdmaEngineBuilder::new(software_device_name().unwrap())
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);
    let listener = bind_listener_with_retry().await;
    let address = connect_addr_for(listener.local_addr());
    let server_resources = resources.clone();
    let (established_tx, established_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(async move {
        let cm_id = listener.get_request().await.unwrap();
        server_resources.require_context(&cm_id).unwrap();
        let qp = server_resources.create_qp(&cm_id, 19, 34).unwrap();
        cm_id.accept(&ConnParam::default()).unwrap();
        listener.await_established().await.unwrap();
        established_tx.send(()).unwrap();
        loop {
            let event = listener.next_event().await.unwrap();
            let event_type = event.event_type();
            event.ack();
            if event_type == CmEventType::Disconnected {
                break;
            }
        }
        drop(qp);
        drop(cm_id);
        drop(listener);
    });

    let mut connect = Box::pin(engine.connect(address));
    let wake = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = futures_util::task::waker(Arc::clone(&wake));
    let mut context = Context::from_waker(&waker);
    assert!(connect.as_mut().poll(&mut context).is_pending());
    established_rx.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while wake.0.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("established connect result was not published");

    drop(connect);
    tokio::time::timeout(Duration::from_secs(15), engine.shutdown())
        .await
        .expect("shutdown did not retire the undelivered connection")
        .unwrap();
    driver_task.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(15), server_task)
        .await
        .expect("peer did not observe undelivered connection teardown")
        .unwrap();
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Terminated);
    assert_eq!(diagnostics.live_connections, 0);

    drop(resources);
    drop(engine);
    let events = recorder.take();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::QueuePair)
            .count(),
        2
    );
}

async fn run_connect_admission_shutdown_barrier(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    const OPERATIONS: usize = 64;

    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(OPERATIONS)
        .cq_capacity(OPERATIONS)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let barrier = resources.pause_next_connect_before_enqueue().unwrap();
    let connect_engine = engine.clone();
    let connect_thread = std::thread::spawn(move || {
        let mut connect =
            Box::pin(async move { connect_engine.connect("127.0.0.1:9".parse().unwrap()).await });
        assert!(poll_once(connect.as_mut()).is_pending());
        connect
    });

    barrier.wait_until_paused().unwrap();
    let paused = engine.diagnostics();
    assert_ne!(paused.lifecycle, RdmaEngineLifecycle::ShutdownRequested);
    assert_eq!(paused.live_connections, 1);
    assert_eq!(paused.registered_operations, 0);
    assert_eq!(paused.available_cq_credits, OPERATIONS);

    let shutdown_engine = engine.clone();
    let shutdown_thread = std::thread::spawn(move || {
        let mut shutdown = Box::pin(async move { shutdown_engine.shutdown().await });
        assert!(poll_once(shutdown.as_mut()).is_pending());
        shutdown
    });
    barrier.wait_until_shutdown_attempted().unwrap();
    barrier.release().unwrap();

    let mut connect = connect_thread.join().expect("connect poll thread panicked");
    let mut shutdown = shutdown_thread
        .join()
        .expect("shutdown poll thread panicked");
    let admitted = engine.diagnostics();
    assert_eq!(admitted.lifecycle, RdmaEngineLifecycle::ShutdownRequested);
    assert_eq!(admitted.live_connections, 1);

    let driver_task = tokio::spawn(driver);
    let (connect_result, shutdown_result) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(connect.as_mut(), shutdown.as_mut())
    })
    .await
    .expect("connect admission shutdown barrier did not drain");
    assert!(matches!(connect_result, Err(Error::DriverShutdown)));
    shutdown_result.unwrap();
    driver_task.await.unwrap().unwrap();

    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Terminated);
    assert!(diagnostics.terminal_error.is_none());
    assert_eq!(diagnostics.live_connections, 0);
    assert_eq!(diagnostics.registered_operations, 0);
    assert_eq!(diagnostics.accepted_operations, 0);
    assert_eq!(diagnostics.available_cq_credits, OPERATIONS);
    assert_eq!(diagnostics.retained_cq_credits, 0);

    drop(resources);
    drop(engine);
}

async fn run_operation_admission_shutdown_barrier(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    const OPERATIONS: usize = 64;

    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(OPERATIONS)
        .cq_capacity(OPERATIONS)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);
    let (server, client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;
    let recorder = DestructionRecorder::arm(64);
    let barrier = resources.pause_next_operation_before_register().unwrap();
    let recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let operation = server.recv(recv, None);
    let operation_thread = std::thread::spawn(move || {
        let mut operation = Box::pin(operation);
        assert!(poll_once(operation.as_mut()).is_pending());
        operation
    });

    barrier.wait_until_paused().unwrap();
    let paused = engine.diagnostics();
    assert_ne!(paused.lifecycle, RdmaEngineLifecycle::ShutdownRequested);
    assert_eq!(paused.live_connections, 2);
    assert_eq!(paused.registered_operations, 0);
    assert_eq!(paused.accepted_operations, 0);
    assert_eq!(paused.available_cq_credits, OPERATIONS);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        0
    );

    let shutdown_engine = engine.clone();
    let shutdown_thread = std::thread::spawn(move || {
        let mut shutdown = Box::pin(async move { shutdown_engine.shutdown().await });
        assert!(poll_once(shutdown.as_mut()).is_pending());
        shutdown
    });
    barrier.wait_until_shutdown_attempted().unwrap();
    barrier.release().unwrap();

    let mut operation = operation_thread
        .join()
        .expect("operation poll thread panicked");
    let mut shutdown = shutdown_thread
        .join()
        .expect("shutdown poll thread panicked");
    let admitted = engine.diagnostics();
    assert_eq!(admitted.lifecycle, RdmaEngineLifecycle::ShutdownRequested);
    assert_eq!(admitted.registered_operations, 1);
    assert_eq!(admitted.accepted_operations, 1);
    assert_eq!(admitted.available_cq_credits, OPERATIONS - 1);
    assert_eq!(admitted.retained_cq_credits, 0);

    let mut server_close = Box::pin(server.close());
    let mut client_close = Box::pin(client.close());
    assert!(poll_once(server_close.as_mut()).is_pending());
    assert!(poll_once(client_close.as_mut()).is_pending());
    resources.transition_connection_to_error(&server).unwrap();

    let ((operation_result, returned), server_result, client_result, shutdown_result) =
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                operation.as_mut(),
                server_close.as_mut(),
                client_close.as_mut(),
                shutdown.as_mut(),
            )
        })
        .await
        .expect("operation admission shutdown barrier did not drain");
    assert!(operation_result.is_err());
    let returned = returned.expect("flush completion must return the registered MR");
    server_result.unwrap();
    client_result.unwrap();
    shutdown_result.unwrap();
    driver_task.await.unwrap().unwrap();

    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Terminated);
    assert!(diagnostics.terminal_error.is_none());
    assert_eq!(diagnostics.live_connections, 0);
    assert_eq!(diagnostics.registered_operations, 0);
    assert_eq!(diagnostics.accepted_operations, 0);
    assert_eq!(diagnostics.available_cq_credits, OPERATIONS);
    assert_eq!(diagnostics.retained_cq_credits, 0);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        0,
        "accepted operation MR must remain registered until its flush CQE"
    );
    drop(returned);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        1
    );

    drop(operation);
    drop(server_close);
    drop(client_close);
    drop(shutdown);
    drop(server);
    drop(client);
    drop(resources);
    drop(engine);
    drop(recorder);
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

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn more_than_one_cm_budget_of_failed_connects_progresses_cooperatively() {
    run_over_budget_failed_connects(CompletionMode::Readiness).await;
    run_over_budget_failed_connects(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn shutdown_retires_an_established_but_undelivered_connect() {
    if !has_software_rdma() {
        return;
    }
    run_shutdown_with_undelivered_connect(CompletionMode::Readiness).await;
    run_shutdown_with_undelivered_connect(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn shutdown_waits_for_connect_admission_publication_in_both_modes() {
    run_connect_admission_shutdown_barrier(CompletionMode::Readiness).await;
    run_connect_admission_shutdown_barrier(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn shutdown_waits_for_operation_registration_and_post_in_both_modes() {
    run_operation_admission_shutdown_barrier(CompletionMode::Readiness).await;
    run_operation_admission_shutdown_barrier(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
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
    assert_eq!(engine.diagnostics().live_connections, 1);
    drop(connect);

    let driver_task = tokio::spawn(driver);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.diagnostics().live_connections == 0 {
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
            if diagnostics.live_connections == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled post-connect route did not clean up after rejection");

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
    assert_eq!(engine.diagnostics().live_connections, 2);

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
    assert_eq!(engine.diagnostics().live_connections, 0);

    let (second_server, second_client) =
        establish_pair(&engine, &resources, RdmaConnectionConfig::default(), true).await;
    assert_eq!(engine.diagnostics().live_connections, 2);
    let second_client_duplicate = second_client.clone();
    let (server_close, client_close, duplicate_close) = tokio::join!(
        second_server.close(),
        second_client.close(),
        second_client_duplicate.close(),
    );
    server_close.unwrap();
    client_close.unwrap();
    duplicate_close.unwrap();
    assert_eq!(engine.diagnostics().live_connections, 0);
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

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn dropping_the_driver_with_live_connections_quarantines_complete_bundles() {
    if !has_software_rdma() {
        return;
    }
    let recorder = DestructionRecorder::arm(64);
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

    driver_task.abort();
    assert!(driver_task.await.unwrap_err().is_cancelled());
    let error = engine.shutdown().await.unwrap_err();
    assert!(matches!(
        error,
        Error::EngineWedged {
            retained_bundles: 2..,
            ..
        }
    ));

    drop(server);
    drop(client);
    drop(resources);
    drop(engine);
    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::QueuePair)
            .count(),
        2,
        "driver loss synchronously destroys only zero-outstanding QPs"
    );
}
