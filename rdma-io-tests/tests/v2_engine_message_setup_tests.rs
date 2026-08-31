use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::message_transport::{TestHelloAttachHook, TestHelloOverride};
use rdma_io::v2::{
    CompletionMode, Error, MessageTransport, MessageTransportBuilder, RdmaConnectionConfig,
    RdmaEngine, RdmaEngineBuilder, RdmaEngineDriver, RdmaListener, RdmaListenerConfig, Result,
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

fn assert_message_future<F: Future<Output = Result<MessageTransport>>>(_: &F) {}

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
    engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(4),
        )
        .await
        .unwrap()
}

async fn establish_messages(
    server: &RdmaEngine,
    client: &RdmaEngine,
) -> (RdmaListener, MessageTransport, MessageTransport) {
    let listener = listen(server).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let accept = MessageTransportBuilder::new().accept_on(&listener);
    let connect = MessageTransportBuilder::new().connect_on(client, address);
    assert_message_future(&accept);
    assert_message_future(&connect);
    let (accepted, connected) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(accept, connect)
    })
    .await
    .expect("message CM establishment timed out");
    (listener, accepted.unwrap(), connected.unwrap())
}

async fn close_pair(
    listener: RdmaListener,
    server: MessageTransport,
    client: MessageTransport,
    server_engine: RdmaEngine,
    client_engine: RdmaEngine,
    server_driver: tokio::task::JoinHandle<Result<()>>,
    client_driver: tokio::task::JoinHandle<Result<()>>,
) {
    let (server_close, client_close) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(server.close(), client.close())
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
        assert_eq!(diagnostics.shared_contexts, 1);
        assert_eq!(diagnostics.shared_protection_domains, 1);
        assert_eq!(diagnostics.shared_completion_queues, 1);
        assert_eq!(diagnostics.shared_cm_event_channels, 1);
        assert_eq!(
            diagnostics.shared_completion_channels,
            usize::from(mode == CompletionMode::Readiness)
        );
        assert_eq!(diagnostics.live_connection_reservations, 1);
        assert_eq!(diagnostics.operations_offered, 36);
        assert_eq!(diagnostics.operations_accepted, 36);
        assert_eq!(diagnostics.operations_completed, 2);
        assert_eq!(diagnostics.accepted_outstanding_operations, 34);
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
    assert_eq!(diagnostics.live_connection_reservations, 0);
    assert_eq!(diagnostics.registered_operations, 0);
    assert_eq!(diagnostics.operations_offered, 0);
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

    tokio::time::timeout(Duration::from_secs(5), async {
        while server_engine.diagnostics().pending_accepts != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled message accept remained registered");

    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            MessageTransportBuilder::new().accept_on(&listener),
            MessageTransportBuilder::new().connect_on(&client_engine, address)
        )
    })
    .await
    .expect("post-cancellation message establishment timed out");
    let server = server.unwrap();
    let client = client.unwrap();
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
    for (hello_override, expected) in [
        (TestHelloOverride::BadMagic, "bad magic"),
        (TestHelloOverride::BadVersion, "unsupported version"),
        (TestHelloOverride::WrongFrameType, "expected HELLO"),
        (
            TestHelloOverride::ZeroReceiveCredits,
            "data_recv_capacity is 0",
        ),
        (
            TestHelloOverride::SmallerMaximumMessage,
            "peer max_message_size",
        ),
    ] {
        let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                MessageTransportBuilder::new().accept_on(&listener),
                MessageTransportBuilder::new()
                    .test_hello_override(hello_override)
                    .connect_on(&client_engine, address)
            )
        })
        .await
        .expect("malformed HELLO connection establishment timed out");
        let server = server.unwrap();
        let client = client.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(15), server.ready())
            .await
            .expect("malformed HELLO did not resolve readiness")
            .unwrap_err();
        assert!(matches!(
            error,
            Error::ProtocolViolation(message) if message.contains(expected)
        ));
        let (server_close, client_close) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(server.close(), client.close())
        })
        .await
        .expect("malformed message close timed out");
        assert!(
            matches!(server_close, Err(Error::ProtocolViolation(_))),
            "unexpected malformed server close: {server_close:?}"
        );
        assert!(
            matches!(client_close, Ok(()) | Err(Error::TransportClosed)),
            "unexpected malformed client close: {client_close:?}"
        );
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

    let first_default = first.clone();
    let default_accept = tokio::spawn(async move { first_default.accept().await });
    wait_pending_accepts(&server_engine, 1).await;
    let first_configured = first.clone();
    let configured = RdmaConnectionConfig::default()
        .max_send_wr(8)
        .max_recv_wr(8);
    let configured_accept = tokio::spawn(async move {
        first_configured
            .accept_with_config(configured.clone())
            .await
    });
    wait_pending_accepts(&server_engine, 2).await;
    let first_message = first.clone();
    let message_accept = tokio::spawn(async move {
        MessageTransportBuilder::new()
            .accept_on(&first_message)
            .await
    });
    wait_pending_accepts(&server_engine, 3).await;
    let second_default = second.clone();
    let independent_accept = tokio::spawn(async move { second_default.accept().await });
    wait_pending_accepts(&server_engine, 4).await;

    let independent_client = tokio::time::timeout(
        Duration::from_secs(15),
        client_engine.connect(second_address),
    )
    .await
    .expect("independent listener client timed out")
    .unwrap();
    let independent_server = tokio::time::timeout(Duration::from_secs(15), independent_accept)
        .await
        .expect("independent listener accept timed out")
        .unwrap()
        .unwrap();
    assert!(!default_accept.is_finished());
    assert!(!configured_accept.is_finished());
    assert!(!message_accept.is_finished());

    let default_client = tokio::time::timeout(
        Duration::from_secs(15),
        client_engine.connect(first_address),
    )
    .await
    .expect("default mixed client timed out")
    .unwrap();
    let default_server = tokio::time::timeout(Duration::from_secs(15), default_accept)
        .await
        .expect("default mixed accept timed out")
        .unwrap()
        .unwrap();
    assert!(!configured_accept.is_finished());
    assert!(!message_accept.is_finished());

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
        match tokio::time::timeout(Duration::from_secs(15), configured_accept).await {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => panic!(
                "configured mixed accept timed out: {:?}",
                server_engine.diagnostics()
            ),
        };
    assert!(!message_accept.is_finished());
    assert_eq!(server_engine.diagnostics().operations_offered, 0);

    let message_client = tokio::time::timeout(
        Duration::from_secs(15),
        MessageTransportBuilder::new().connect_on(&client_engine, first_address),
    )
    .await
    .expect("mixed message client timed out")
    .unwrap();
    let message_server = tokio::time::timeout(Duration::from_secs(15), message_accept)
        .await
        .expect("mixed message accept timed out")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) =
            tokio::join!(message_server.ready(), message_client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("mixed message accept HELLO timed out");
    assert_eq!(server_engine.diagnostics().operations_offered, 36);

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
    assert_eq!(client_engine.diagnostics().operations_offered, 0);
    assert_eq!(client_engine.diagnostics().connections_opened, 0);
    assert_eq!(server_engine.diagnostics().operations_offered, 0);

    let client_driver = tokio::spawn(client_driver);
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(accept, connect)
    })
    .await
    .expect("message setup did not progress after polling the withheld driver");
    let server = server.unwrap().unwrap();
    let client = client.unwrap().unwrap();
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

async fn wait_pending_accepts(engine: &RdmaEngine, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while engine.diagnostics().pending_accepts != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {expected} pending accept waiters"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_delivered_during_ready_work_attachment_is_processed() {
    if !has_software_rdma() {
        return;
    }
    let (server_engine, server_driver) = build_engine(CompletionMode::Readiness).await;
    let (client_engine, client_driver) = build_engine(CompletionMode::Readiness).await;
    let listener = listen(&server_engine).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let hook = TestHelloAttachHook::new();

    let accept_listener = listener.clone();
    let accept = tokio::spawn(async move {
        MessageTransportBuilder::new()
            .accept_on(&accept_listener)
            .await
    });
    let connect_engine = client_engine.clone();
    let connect_hook = hook.clone();
    let connect = tokio::spawn(async move {
        MessageTransportBuilder::new()
            .test_hello_attach_hook(connect_hook)
            .connect_on(&connect_engine, address)
            .await
    });

    let attached_hook = hook.clone();
    let attached =
        tokio::task::spawn_blocking(move || attached_hook.wait_until_ready_work_attached())
            .await
            .unwrap();
    if attached.is_err() {
        hook.release();
    }
    attached.unwrap();

    hook.deliver_hello().unwrap();
    let hello_hook = hook.clone();
    let hello = tokio::task::spawn_blocking(move || hello_hook.wait_until_hello_processed())
        .await
        .unwrap();
    hook.release();
    if let Err(error) = hello {
        panic!(
            "{error}; server={:?}; client={:?}",
            server_engine.diagnostics(),
            client_engine.diagnostics()
        );
    }

    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(accept, connect)
    })
    .await
    .expect("message attachment hook did not resume");
    let server = server.unwrap().unwrap();
    let client = client.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("HELLO delivered during attachment was lost");

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
