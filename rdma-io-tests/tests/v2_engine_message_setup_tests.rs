use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::{
    CompletionMode, Error, MessageTransport, MessageTransportBuilder, MessageTransportDriver,
    RdmaConnectionConfig, RdmaEngine, RdmaEngineBuilder, RdmaEngineDriver, RdmaListener,
    RdmaListenerConfig, Result,
};
use rdma_io_tests::engine_test_helpers::{
    DrivenMessageTransport, peer_hello_frame, send_peer_frame,
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

fn drive(pair: (MessageTransport, MessageTransportDriver)) -> DrivenMessageTransport {
    DrivenMessageTransport::new(pair.0, pair.1)
}

fn build_engine_unspawned(mode: CompletionMode) -> (RdmaEngine, RdmaEngineDriver) {
    let device = software_device_name().expect("software RDMA device");
    RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap()
}

async fn build_engine(mode: CompletionMode) -> (RdmaEngine, tokio::task::JoinHandle<Result<()>>) {
    let (engine, driver) = build_engine_unspawned(mode);
    (engine, tokio::spawn(driver))
}

async fn listen(engine: &RdmaEngine) -> RdmaListener {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match engine
                .listen(
                    "0.0.0.0:0".parse().unwrap(),
                    RdmaListenerConfig::default().backlog(4),
                )
                .await
            {
                Ok(listener) => return listener,
                Err(Error::Verbs(error)) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("listener setup failed: {error}"),
            }
        }
    })
    .await
    .expect("listener setup remained busy")
}

async fn establish_messages(
    server: &RdmaEngine,
    client: &RdmaEngine,
) -> (RdmaListener, DrivenMessageTransport, DrivenMessageTransport) {
    let listener = listen(server).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let accept = MessageTransportBuilder::new().accept_on(&listener);
    let connect = MessageTransportBuilder::new().connect_on(client, address);
    let (accepted, connected) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(accept, connect)
    })
    .await
    .expect("message CM establishment timed out");
    (
        listener,
        drive(accepted.unwrap()),
        drive(connected.unwrap()),
    )
}

async fn close_pair(
    listener: RdmaListener,
    server: DrivenMessageTransport,
    client: DrivenMessageTransport,
    server_engine: RdmaEngine,
    client_engine: RdmaEngine,
    server_driver: tokio::task::JoinHandle<Result<()>>,
    client_driver: tokio::task::JoinHandle<Result<()>>,
) {
    let (server_close, client_close) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(server.shutdown(), client.shutdown())
    })
    .await
    .expect("message close timed out");
    assert_clean_or_disconnected(server_close);
    assert_clean_or_disconnected(client_close);
    listener.close().await.unwrap();
    server_engine.shutdown().await.unwrap();
    client_engine.shutdown().await.unwrap();
    server_driver.await.unwrap().unwrap();
    client_driver.await.unwrap().unwrap();
}

async fn run_success(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine(mode).await;
    let (client_engine, client_driver) = build_engine(mode).await;
    let (listener, server, client) = establish_messages(&server_engine, &client_engine).await;

    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("engine-routed HELLO negotiation timed out");

    for engine in [&server_engine, &client_engine] {
        let diagnostics = engine.diagnostics();
        assert_eq!(diagnostics.live_connections, 1);
        assert_eq!(diagnostics.registered_operations, 34);
        assert_eq!(diagnostics.accepted_operations, 34);
    }

    close_pair(
        listener,
        server,
        client,
        server_engine,
        client_engine,
        server_driver,
        client_driver,
    )
    .await;
}

async fn run_capacity_rejection(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(2)
        .maximum_inflight_operations(6)
        .cq_capacity(6)
        .build()
        .unwrap();
    let driver = tokio::spawn(driver);
    let result = MessageTransportBuilder::new()
        .send_buffers(1)
        .recv_buffers(1)
        .connect_on(&engine, "127.0.0.1:1".parse().unwrap())
        .await;
    assert!(matches!(result, Err(Error::InvalidConfig(_))));
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.live_connections, 0);
    assert_eq!(diagnostics.registered_operations, 0);
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
}

async fn run_cancelled_accept(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine(mode).await;
    let (client_engine, client_driver) = build_engine(mode).await;
    let listener = listen(&server_engine).await;
    let mut cancelled = Box::pin(MessageTransportBuilder::new().accept_on(&listener));
    assert!(poll_once(cancelled.as_mut()).is_pending());
    drop(cancelled);

    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            MessageTransportBuilder::new().accept_on(&listener),
            MessageTransportBuilder::new().connect_on(&client_engine, address)
        )
    })
    .await
    .expect("post-cancellation message establishment timed out");
    let server = drive(server.unwrap());
    let client = drive(client.unwrap());
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("post-cancellation HELLO timed out");
    close_pair(
        listener,
        server,
        client,
        server_engine,
        client_engine,
        server_driver,
        client_driver,
    )
    .await;
}

async fn run_malformed_hello(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine(mode).await;
    let (client_engine, client_driver) = build_engine(mode).await;
    let listener = listen(&server_engine).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let mut bad_magic = peer_hello_frame(32, 64 * 1024);
    bad_magic[0] ^= 0xff;
    let mut bad_version = peer_hello_frame(32, 64 * 1024);
    bad_version[4] = 2;
    let mut wrong_type = peer_hello_frame(32, 64 * 1024);
    wrong_type[5] = 0;
    for (frame, expected) in [
        (bad_magic, "bad magic"),
        (bad_version, "unsupported version"),
        (wrong_type, "expected HELLO"),
        (peer_hello_frame(0, 64 * 1024), "data_recv_capacity is 0"),
        (peer_hello_frame(32, 64 * 1024 - 1), "peer max_message_size"),
    ] {
        let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                MessageTransportBuilder::new().accept_on(&listener),
                MessageTransportBuilder::new().connect_on(&client_engine, address)
            )
        })
        .await
        .expect("malformed HELLO connection establishment timed out");
        let (server, server_message_driver) = server.unwrap();
        let (client, client_message_driver) = client.unwrap();
        let peer = client.test_connection().unwrap();
        let server = DrivenMessageTransport::new(server, server_message_driver);
        match send_peer_frame(&peer, &frame).await {
            Ok(()) | Err(Error::TransportClosed) => {}
            Err(error) => panic!("failed to inject malformed HELLO ({expected}): {error:?}"),
        }
        let error = tokio::time::timeout(Duration::from_secs(15), server.ready())
            .await
            .expect("malformed HELLO did not resolve readiness")
            .unwrap_err();
        assert!(matches!(
            error,
            Error::ProtocolViolation(message) if message.contains(expected)
        ));
        let server_close = tokio::time::timeout(Duration::from_secs(15), server.shutdown())
            .await
            .expect("malformed server shutdown timed out");
        assert!(
            matches!(server_close, Err(Error::ProtocolViolation(_))),
            "unexpected malformed server close: {server_close:?}"
        );
        drop(client_message_driver);
        let _ = client.close().await;
        drop(peer);
        drop(client);
    }
    listener.close().await.unwrap();
    server_engine.shutdown().await.unwrap();
    client_engine.shutdown().await.unwrap();
    server_driver.await.unwrap().unwrap();
    client_driver.await.unwrap().unwrap();
}

async fn run_mixed_accept_order(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine(mode).await;
    let (client_engine, client_driver) = build_engine(mode).await;
    let first = listen(&server_engine).await;
    let second = listen(&server_engine).await;
    let first_address = connect_addr_for(Some(first.local_addr().unwrap()));
    let second_address = connect_addr_for(Some(second.local_addr().unwrap()));

    let mut default_accept = Box::pin(first.accept());
    assert!(poll_once(default_accept.as_mut()).is_pending());
    let configured = RdmaConnectionConfig::default()
        .max_send_wr(8)
        .max_recv_wr(8);
    let mut configured_accept = Box::pin(first.accept_with_config(configured.clone()));
    assert!(poll_once(configured_accept.as_mut()).is_pending());
    let mut message_accept = Box::pin(MessageTransportBuilder::new().accept_on(&first));
    assert!(poll_once(message_accept.as_mut()).is_pending());
    let mut independent_accept = Box::pin(second.accept());
    assert!(poll_once(independent_accept.as_mut()).is_pending());

    let independent_client = tokio::time::timeout(
        Duration::from_secs(15),
        client_engine.connect(second_address),
    )
    .await
    .expect("independent listener client timed out")
    .unwrap();
    let independent_server =
        tokio::time::timeout(Duration::from_secs(15), independent_accept.as_mut())
            .await
            .expect("independent listener accept timed out")
            .unwrap();
    assert!(poll_once(default_accept.as_mut()).is_pending());
    assert!(poll_once(configured_accept.as_mut()).is_pending());
    assert!(poll_once(message_accept.as_mut()).is_pending());

    let default_client = tokio::time::timeout(
        Duration::from_secs(15),
        client_engine.connect(first_address),
    )
    .await
    .expect("default mixed client timed out")
    .unwrap();
    let default_server = tokio::time::timeout(Duration::from_secs(15), default_accept.as_mut())
        .await
        .expect("default mixed accept timed out")
        .unwrap();
    assert!(poll_once(configured_accept.as_mut()).is_pending());
    assert!(poll_once(message_accept.as_mut()).is_pending());

    let configured = RdmaConnectionConfig::default()
        .max_send_wr(8)
        .max_recv_wr(8);
    let configured_client = tokio::time::timeout(
        Duration::from_secs(15),
        client_engine.connect_with_config(first_address, configured),
    )
    .await
    .expect("configured mixed client timed out")
    .unwrap();
    let configured_server =
        match tokio::time::timeout(Duration::from_secs(15), configured_accept.as_mut()).await {
            Ok(result) => result.unwrap(),
            Err(_) => panic!(
                "configured mixed accept timed out: {:?}",
                server_engine.diagnostics()
            ),
        };
    assert!(poll_once(message_accept.as_mut()).is_pending());

    let message_client = tokio::time::timeout(
        Duration::from_secs(15),
        MessageTransportBuilder::new().connect_on(&client_engine, first_address),
    )
    .await
    .expect("mixed message client timed out")
    .unwrap();
    let message_server = tokio::time::timeout(Duration::from_secs(15), message_accept.as_mut())
        .await
        .expect("mixed message accept timed out")
        .unwrap();
    let message_client = drive(message_client);
    let message_server = drive(message_server);
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) =
            tokio::join!(message_server.ready(), message_client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("mixed message accept HELLO timed out");

    let (message_server_close, message_client_close) =
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(message_server.close(), message_client.close())
        })
        .await
        .expect("mixed message close timed out");
    assert_clean_or_disconnected(message_server_close);
    assert_clean_or_disconnected(message_client_close);
    for connection in [
        independent_server,
        independent_client,
        default_server,
        default_client,
        configured_server,
        configured_client,
    ] {
        tokio::time::timeout(Duration::from_secs(15), connection.close())
            .await
            .expect("mixed low-level close timed out")
            .unwrap();
    }
    first.close().await.unwrap();
    second.close().await.unwrap();
    server_engine.shutdown().await.unwrap();
    client_engine.shutdown().await.unwrap();
    server_driver.await.unwrap().unwrap();
    client_driver.await.unwrap().unwrap();
}

async fn run_driver_withholding(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine(mode).await;
    let (client_engine, client_driver) = build_engine_unspawned(mode);
    let listener = listen(&server_engine).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));

    let accept_listener = listener.clone();
    let accept = tokio::spawn(async move {
        MessageTransportBuilder::new()
            .accept_on(&accept_listener)
            .await
    });
    let connect_engine = client_engine.clone();
    let connect = tokio::spawn(async move {
        MessageTransportBuilder::new()
            .connect_on(&connect_engine, address)
            .await
    });
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert!(!accept.is_finished());
    assert!(!connect.is_finished());

    let client_driver = tokio::spawn(client_driver);
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(accept, connect)
    })
    .await
    .expect("message setup did not progress after polling the withheld driver");
    let (server, server_message_driver) = server.unwrap().unwrap();
    let (client, client_message_driver) = client.unwrap().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), async {
            tokio::join!(server.ready(), client.ready())
        })
        .await
        .is_err(),
        "HELLO unexpectedly progressed without polling message drivers"
    );
    let server = DrivenMessageTransport::new(server, server_message_driver);
    let client = DrivenMessageTransport::new(client, client_message_driver);
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("withheld-driver HELLO negotiation timed out");

    close_pair(
        listener,
        server,
        client,
        server_engine,
        client_engine,
        server_driver,
        client_driver,
    )
    .await;
}

async fn run_message_driver_hello_timeout(mode: CompletionMode) {
    let (server_engine, server_engine_driver) = build_engine(mode).await;
    let (client_engine, client_engine_driver) = build_engine(mode).await;
    let listener = listen(&server_engine).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            MessageTransportBuilder::new()
                .hello_deadline(Duration::from_millis(50))
                .accept_on(&listener),
            MessageTransportBuilder::new()
                .hello_deadline(Duration::from_millis(50))
                .connect_on(&client_engine, address)
        )
    })
    .await
    .expect("message setup timed out");
    let (server, server_message_driver) = server.unwrap();
    let (client, client_message_driver) = client.unwrap();
    let server_message_driver = tokio::spawn(server_message_driver);

    let error = tokio::time::timeout(Duration::from_secs(5), server.ready())
        .await
        .expect("message driver HELLO deadline did not fire")
        .unwrap_err();
    assert!(matches!(
        error,
        Error::ProtocolViolation(message) if message == "HELLO handshake timeout"
    ));
    assert!(matches!(
        server_message_driver.await.unwrap(),
        Err(Error::ProtocolViolation(message)) if message == "HELLO handshake timeout"
    ));
    drop(client_message_driver);
    let _ = tokio::join!(server.close(), client.close());
    listener.close().await.unwrap();
    server_engine.shutdown().await.unwrap();
    client_engine.shutdown().await.unwrap();
    server_engine_driver.await.unwrap().unwrap();
    client_engine_driver.await.unwrap().unwrap();
}

fn assert_clean_or_disconnected(result: Result<()>) {
    assert!(
        matches!(result, Ok(()) | Err(Error::TransportClosed)),
        "unexpected message close result: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_message_setup_and_hello() {
    run_success(CompletionMode::Readiness).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_message_setup_and_hello() {
    run_success(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_message_capacity_fails_before_reservation() {
    run_capacity_rejection(CompletionMode::Readiness).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_message_capacity_fails_before_reservation() {
    run_capacity_rejection(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_cancelled_message_accept_does_not_overtake() {
    run_cancelled_accept(CompletionMode::Readiness).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_cancelled_message_accept_does_not_overtake() {
    run_cancelled_accept(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_malformed_hello_is_connection_local() {
    run_malformed_hello(CompletionMode::Readiness).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_malformed_hello_is_connection_local() {
    run_malformed_hello(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_mixed_accepts_preserve_registration_order() {
    run_mixed_accept_order(CompletionMode::Readiness).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_mixed_accepts_preserve_registration_order() {
    run_mixed_accept_order(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_setup_requires_the_owning_driver_to_be_polled() {
    run_driver_withholding(CompletionMode::Readiness).await;
    run_driver_withholding(CompletionMode::Polling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_driver_owns_hello_timeout_in_both_completion_modes() {
    if !has_software_rdma() {
        return;
    }
    run_message_driver_hello_timeout(CompletionMode::Readiness).await;
    run_message_driver_hello_timeout(CompletionMode::Polling).await;
}
