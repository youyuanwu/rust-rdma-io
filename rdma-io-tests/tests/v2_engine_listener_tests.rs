use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, RdmaConnection, RdmaConnectionConfig, RdmaEngine,
    RdmaEngineBuilder, RdmaListener, RdmaListenerConfig,
};
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

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

async fn exchange(server: &RdmaConnection, client: &RdmaConnection, value: u8) {
    let recv = server.register_memory(16, AccessIntent::LocalOnly).unwrap();
    let mut send = client.register_memory(16, AccessIntent::LocalOnly).unwrap();
    send.as_mut_slice()[0] = value;
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(server.recv(recv, None), client.send(send, None));
    recv_result.unwrap();
    send_result.unwrap();
    assert_eq!(recv.unwrap().as_slice()[0], value);
    assert!(send.is_some());
}

async fn accept_pair(
    listener: &RdmaListener,
    clients: &RdmaEngine,
    configured: bool,
) -> (RdmaConnection, RdmaConnection) {
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let accept = async {
        if configured {
            listener
                .accept_with_config(
                    RdmaConnectionConfig::default()
                        .max_send_wr(8)
                        .max_recv_wr(8),
                )
                .await
        } else {
            listener.accept().await
        }
    };
    let connect = async {
        if configured {
            clients
                .connect_with_config(
                    address,
                    RdmaConnectionConfig::default()
                        .max_send_wr(8)
                        .max_recv_wr(8),
                )
                .await
        } else {
            clients.connect(address).await
        }
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server, client) = tokio::join!(accept, connect);
        (server.unwrap(), client.unwrap())
    })
    .await
    .expect("engine listener establishment timed out")
}

async fn run_basic_listener(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);

    let invalid = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(0),
        )
        .await;
    assert!(matches!(invalid, Err(Error::InvalidConfig(_))));
    assert_eq!(server_engine.diagnostics().listener_count, 0);
    assert_eq!(server_engine.diagnostics().listeners_created, 0);

    let listener = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(2),
        )
        .await
        .unwrap();
    let failing_address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let (failed_accept, failed_connect) = tokio::join!(
        listener.accept_with_test_setup_failure("injected setup failure"),
        client_engine.connect(failing_address)
    );
    assert!(matches!(failed_accept, Err(Error::InvalidConfig(_))));
    assert!(failed_connect.is_err());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let diagnostics = server_engine.diagnostics();
            if diagnostics.accept_setup_failures == 1
                && diagnostics.inbound_rejected_setup_failure == 1
                && diagnostics.live_connection_reservations == 0
                && diagnostics.selected_accepts == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("selected setup failure did not reject and retire exactly once");
    let (server_default, client_default) = accept_pair(&listener, &client_engine, false).await;
    let (server_configured, client_configured) = accept_pair(&listener, &client_engine, true).await;
    let post_accept = server_engine.diagnostics();
    assert_eq!(post_accept.operations_offered, 0);
    assert_eq!(post_accept.accepted_outstanding_operations, 0);

    exchange(&server_default, &client_default, 7).await;
    exchange(&server_configured, &client_configured, 9).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let mut queued_connects = tokio::task::JoinSet::new();
    for _ in 0..2 {
        let engine = client_engine.clone();
        queued_connects.spawn(async move { engine.connect(address).await });
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        while server_engine.diagnostics().queued_inbound_requests != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("listener userspace backlog did not fill");
    let overflow_engine = client_engine.clone();
    queued_connects.spawn(async move { overflow_engine.connect(address).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        while server_engine.diagnostics().inbound_rejected_backlog_full != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("listener userspace backlog did not reject overflow");
    let (queued_server_a, queued_server_b) = tokio::join!(listener.accept(), listener.accept());
    let queued_servers = [queued_server_a.unwrap(), queued_server_b.unwrap()];
    let mut queued_clients = Vec::new();
    let mut backlog_rejections = 0;
    while let Some(result) = queued_connects.join_next().await {
        match result.unwrap() {
            Ok(connection) => queued_clients.push(connection),
            Err(_) => backlog_rejections += 1,
        }
    }
    assert_eq!(queued_clients.len(), 2);
    assert_eq!(backlog_rejections, 1);

    let diagnostics = server_engine.diagnostics();
    assert_eq!(diagnostics.listener_count, 1);
    assert_eq!(diagnostics.inbound_requests_accepted, 4);
    assert_eq!(diagnostics.shared_cm_event_channels, 1);
    assert_eq!(diagnostics.queued_inbound_requests, 0);
    assert_eq!(diagnostics.pending_accepts, 0);
    assert_eq!(diagnostics.selected_accepts, 0);

    for connection in [&server_default, &server_configured] {
        connection.close().await.unwrap();
    }
    for connection in &queued_servers {
        connection.close().await.unwrap();
    }
    for connection in [&client_default, &client_configured] {
        connection.close().await.unwrap();
    }
    for connection in &queued_clients {
        connection.close().await.unwrap();
    }
    listener.close().await.unwrap();
    server_engine.shutdown().await.unwrap();
    client_engine.shutdown().await.unwrap();
    server_task.await.unwrap().unwrap();
    client_task.await.unwrap().unwrap();
}

async fn run_two_listener_backlog_and_capacity(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(5)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let first = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(2),
        )
        .await
        .unwrap();
    let second = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(2),
        )
        .await
        .unwrap();
    let first_addr = connect_addr_for(Some(first.local_addr().unwrap()));
    let second_addr = connect_addr_for(Some(second.local_addr().unwrap()));

    let mut connects = tokio::task::JoinSet::new();
    for address in [first_addr, first_addr, second_addr, second_addr] {
        let engine = client_engine.clone();
        connects.spawn(async move { engine.connect(address).await });
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let diagnostics = server_engine.diagnostics();
            if diagnostics.listener_count == 2
                && diagnostics.queued_inbound_requests == 4
                && diagnostics.live_connection_reservations == 4
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "listeners did not fill their independent backlogs: {:?}",
            server_engine.diagnostics()
        )
    });
    let engine = client_engine.clone();
    connects.spawn(async move { engine.connect(first_addr).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if server_engine
                .diagnostics()
                .inbound_rejected_connection_capacity
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "aggregate connection capacity did not reject the fifth child: {:?}",
            server_engine.diagnostics()
        )
    });
    match server_engine.connect(first_addr).await {
        Err(Error::CapacityExhausted) => {}
        Err(error) => panic!("unexpected outbound capacity error: {error}"),
        Ok(_) => panic!("outbound request oversubscribed listener-held capacity"),
    }
    assert_eq!(server_engine.diagnostics().live_connection_reservations, 4);

    let accepted = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            first.accept(),
            second.accept(),
            first.accept(),
            second.accept()
        )
    })
    .await
    .expect("independent listeners stopped making progress");
    let servers = [
        accepted.0.unwrap(),
        accepted.1.unwrap(),
        accepted.2.unwrap(),
        accepted.3.unwrap(),
    ];

    let mut clients = Vec::new();
    let mut rejected = 0;
    while let Some(result) = connects.join_next().await {
        match result.unwrap() {
            Ok(connection) => clients.push(connection),
            Err(error) => {
                rejected += 1;
                assert!(
                    matches!(error, Error::Verbs(_) | Error::DriverShutdown),
                    "unexpected rejected-client error: {error}"
                );
            }
        }
    }
    assert_eq!(clients.len(), 4);
    assert_eq!(rejected, 1);
    assert_eq!(server_engine.diagnostics().inbound_requests_accepted, 4);

    for connection in &servers {
        connection.close().await.unwrap();
    }
    for connection in &clients {
        connection.close().await.unwrap();
    }
    first.close().await.unwrap();
    second.close().await.unwrap();
    server_engine.shutdown().await.unwrap();
    client_engine.shutdown().await.unwrap();
    server_task.await.unwrap().unwrap();
    client_task.await.unwrap().unwrap();
}

async fn run_accept_cancellation_and_live_shutdown(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (server_engine, server_driver) = RdmaEngineBuilder::new(device.clone())
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .build()
        .unwrap();
    let (client_engine, client_driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(128)
        .cq_capacity(128)
        .build()
        .unwrap();
    let server_task = tokio::spawn(server_driver);
    let client_task = tokio::spawn(client_driver);
    let listener = server_engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(1),
        )
        .await
        .unwrap();
    let cancelled_listener = listener.clone();
    let cancelled = tokio::spawn(async move { cancelled_listener.accept().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server_engine.diagnostics().pending_accepts != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accept waiter was not registered");
    cancelled.abort();
    match cancelled.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("accept task completed before cancellation"),
    }

    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let connect_engine = client_engine.clone();
    let connect = tokio::spawn(async move { connect_engine.connect(address).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let diagnostics = server_engine.diagnostics();
            if diagnostics.accept_cancellations_before_selection == 1
                && diagnostics.queued_inbound_requests == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled accept was not removed before child selection");
    let server_connection = listener.accept().await.unwrap();
    let client_connection = connect.await.unwrap().unwrap();

    let mut cancelled_after_accept = Box::pin(listener.accept());
    assert!(poll_once(cancelled_after_accept.as_mut()).is_pending());
    let second_address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let second_engine = client_engine.clone();
    let second_connect = tokio::spawn(async move { second_engine.connect(second_address).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let diagnostics = server_engine.diagnostics();
            if diagnostics.inbound_requests_accepted == 2 && diagnostics.selected_accepts == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("selected accept did not reach undelivered ESTABLISHED");
    drop(cancelled_after_accept);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let diagnostics = server_engine.diagnostics();
            if diagnostics.accept_cancellations_after_selection == 1
                && diagnostics.selected_accepts == 0
                && diagnostics.live_connection_reservations == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "post-accept cancellation did not finish ordered close: {:?}",
            server_engine.diagnostics()
        )
    });
    let second_client = tokio::time::timeout(Duration::from_secs(15), second_connect)
        .await
        .expect("cancelled server accept left client connect pending")
        .unwrap()
        .ok();
    drop(second_client);
    tokio::time::timeout(Duration::from_secs(10), async {
        while client_engine.diagnostics().live_connection_reservations != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled accept client connection did not retire");

    let mut selected_during_close = Box::pin(listener.accept());
    assert!(poll_once(selected_during_close.as_mut()).is_pending());
    let pending_listener = listener.clone();
    let pending_accept = tokio::spawn(async move { pending_listener.accept().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server_engine.diagnostics().pending_accepts != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("listener-close waiters were not registered");
    let close_address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let close_engine = client_engine.clone();
    let close_connect = tokio::spawn(async move { close_engine.connect(close_address).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let diagnostics = server_engine.diagnostics();
            if diagnostics.inbound_requests_accepted == 3
                && diagnostics.selected_accepts == 1
                && diagnostics.pending_accepts == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "listener-close selected pair did not reach ESTABLISHED: {:?}",
            server_engine.diagnostics()
        )
    });
    let mut close_listener = Box::pin(listener.close());
    assert!(poll_once(close_listener.as_mut()).is_pending());
    close_listener.await.unwrap();
    let selected_result = selected_during_close.await;
    assert!(matches!(selected_result, Err(Error::TransportClosed)));
    match pending_accept.await.unwrap() {
        Err(Error::TransportClosed) => {}
        Err(error) => panic!("unexpected listener-close accept error: {error}"),
        Ok(_) => panic!("listener close completed a pending accept successfully"),
    }
    let close_client = tokio::time::timeout(Duration::from_secs(15), close_connect)
        .await
        .expect("listener close left selected client connect pending")
        .unwrap()
        .ok();

    let recv_connection = server_connection.clone();
    let pending_recv = tokio::spawn(async move {
        let mr = recv_connection
            .register_memory(16, AccessIntent::LocalOnly)
            .unwrap();
        recv_connection.recv(mr, None).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server_engine.diagnostics().accepted_outstanding_operations != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("receive was not accepted before shutdown");

    let (server_shutdown, client_shutdown) =
        tokio::join!(server_engine.shutdown(), client_engine.shutdown());
    server_shutdown.unwrap();
    client_shutdown.unwrap();
    let (recv_result, returned) = pending_recv.await.unwrap();
    assert!(recv_result.is_err());
    assert!(returned.is_some());
    server_task.await.unwrap().unwrap();
    client_task.await.unwrap().unwrap();
    assert_eq!(server_engine.diagnostics().live_connection_reservations, 0);
    assert_eq!(client_engine.diagnostics().live_connection_reservations, 0);
    drop(server_connection);
    drop(client_connection);
    drop(close_client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_api_and_shared_resources_work_in_both_modes() {
    run_basic_listener(CompletionMode::Readiness).await;
    run_basic_listener(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_listeners_have_independent_backlogs_and_shared_capacity() {
    run_two_listener_backlog_and_capacity(CompletionMode::Readiness).await;
    run_two_listener_backlog_and_capacity(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_accepts_do_not_steal_children_and_shutdown_closes_live_connections() {
    run_accept_cancellation_and_live_shutdown(CompletionMode::Readiness).await;
    run_accept_cancellation_and_live_shutdown(CompletionMode::Polling).await;
}
