use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use std::{collections::HashSet, iter};

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::test_support::{DestructionKind, DestructionRecorder, TestSteadyFrame};
use rdma_io::v2::{
    CompletionMode, Error, MessageTransport, MessageTransportBuilder, RdmaEngine,
    RdmaEngineBuilder, RdmaListener, RdmaListenerConfig, Result,
};
use rdma_io::wc::WcOpcode;
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

#[derive(Clone, Copy)]
struct MessageConfig {
    sends: usize,
    recvs: usize,
    size: usize,
}

impl Default for MessageConfig {
    fn default() -> Self {
        Self {
            sends: 4,
            recvs: 4,
            size: 1024,
        }
    }
}

fn software_device_name() -> Option<String> {
    let list = RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn builder(config: MessageConfig) -> MessageTransportBuilder {
    MessageTransportBuilder::new()
        .send_buffers(config.sends)
        .recv_buffers(config.recvs)
        .buffer_size(config.size)
}

fn build_engine(
    mode: CompletionMode,
    quantum: usize,
    drain_deadline: Duration,
) -> (RdmaEngine, tokio::task::JoinHandle<Result<()>>) {
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(512)
        .cq_capacity(512)
        .ready_connection_quantum(quantum)
        .connection_drain_deadline(drain_deadline)
        .shutdown_deadline(Duration::from_secs(5))
        .build()
        .unwrap();
    (engine, tokio::spawn(driver))
}

async fn listen(engine: &RdmaEngine) -> RdmaListener {
    engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(8),
        )
        .await
        .unwrap()
}

async fn establish_on(
    server_engine: &RdmaEngine,
    client_engine: &RdmaEngine,
    config: MessageConfig,
) -> (RdmaListener, MessageTransport, MessageTransport) {
    let listener = listen(server_engine).await;
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            builder(config).accept_on(&listener),
            builder(config).connect_on(client_engine, address)
        )
    })
    .await
    .expect("message establishment timed out");
    let server = server.unwrap();
    let client = client.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
        server_ready.unwrap();
        client_ready.unwrap();
    })
    .await
    .expect("message HELLO timed out");
    (listener, server, client)
}

fn assert_clean_close(result: Result<()>) {
    assert!(
        matches!(result, Ok(()) | Err(Error::TransportClosed)),
        "unexpected clean close result: {result:?}"
    );
}

async fn close_connection_pair(
    listener: RdmaListener,
    server: MessageTransport,
    client: MessageTransport,
) {
    let (server_close, client_close) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(server.close(), client.close())
    })
    .await
    .expect("message close timed out");
    assert_clean_close(server_close);
    assert_clean_close(client_close);
    listener.close().await.unwrap();
}

async fn shutdown_engine(engine: RdmaEngine, driver: tokio::task::JoinHandle<Result<()>>) {
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
}

async fn run_boundaries_and_reuse(mode: CompletionMode) {
    let (server_engine, server_driver) = build_engine(mode, 2, Duration::from_secs(5));
    let (client_engine, client_driver) = build_engine(mode, 2, Duration::from_secs(5));
    let config = MessageConfig {
        sends: 2,
        recvs: 2,
        size: 256,
    };
    let (listener, server, client) = establish_on(&server_engine, &client_engine, config).await;

    assert_eq!(client.test_negotiated_credits().unwrap(), (2, 0));
    client.send(b"").await.unwrap();
    let empty = server.recv().await.unwrap();
    assert!(empty.is_empty());
    drop(empty);

    let maximum = vec![0x5a; config.size];
    client.send(&maximum).await.unwrap();
    let message = server.recv().await.unwrap();
    assert_eq!(&*message, maximum.as_slice());
    drop(message);
    assert!(matches!(
        client.send(&vec![0; config.size + 1]).await,
        Err(Error::MessageTooLarge {
            size: 257,
            capacity: 256
        })
    ));

    for sequence in 0..32u32 {
        let payload = sequence.to_le_bytes();
        client.send(&payload).await.unwrap();
        let message = server.recv().await.unwrap();
        assert_eq!(&*message, payload.as_slice());
        drop(message);
    }

    let client = Arc::new(client);
    let server = Arc::new(server);
    let senders = (0..16u32)
        .map(|sequence| {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                client.send(&sequence.to_le_bytes()).await.unwrap();
            })
        })
        .collect::<Vec<_>>();
    let receivers = iter::repeat_with(|| {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let message = server.recv().await.unwrap();
            let value = u32::from_le_bytes(message.as_ref().try_into().unwrap());
            drop(message);
            value
        })
    })
    .take(16)
    .collect::<Vec<_>>();
    for sender in senders {
        sender.await.unwrap();
    }
    let mut received = HashSet::new();
    for receiver in receivers {
        received.insert(receiver.await.unwrap());
    }
    assert_eq!(received, (0..16u32).collect());

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client.test_available_send_buffers().unwrap() == config.sends
                && client.test_negotiated_credits().unwrap() == (config.recvs, 0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registered message buffers or negotiated credits were not reused");

    let server = Arc::try_unwrap(server).ok().expect("single server owner");
    let client = Arc::try_unwrap(client).ok().expect("single client owner");
    close_connection_pair(listener, server, client).await;
    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}

async fn run_held_repost(mode: CompletionMode) {
    let (server_engine, server_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let (client_engine, client_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let config = MessageConfig {
        sends: 2,
        recvs: 1,
        size: 64,
    };
    let (listener, server, client) = establish_on(&server_engine, &client_engine, config).await;

    let receive_control = server_engine
        .test_resources()
        .unwrap()
        .pause_ready_work()
        .unwrap();
    client.send(b"first").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), server.recv())
            .await
            .is_err(),
        "server message delivery must remain engine-ready work"
    );
    receive_control.release();
    let first = server.recv().await.unwrap();
    assert_eq!(&*first, b"first");
    assert_eq!(client.test_negotiated_credits().unwrap(), (0, 1));
    let client = Arc::new(client);
    let second_client = Arc::clone(&client);
    let second = tokio::spawn(async move { second_client.send(b"second").await });
    tokio::task::yield_now().await;
    assert!(
        !second.is_finished(),
        "held receive must apply backpressure"
    );

    let ready_control = server_engine
        .test_resources()
        .unwrap()
        .pause_ready_work()
        .unwrap();
    let accepted_before_drop = server_engine.diagnostics().accepted_outstanding_operations;
    drop(first);
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.test_pending_ready_work().unwrap() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ReceivedMessage drop did not publish repost work");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(!second.is_finished());
    assert_eq!(
        server_engine.diagnostics().accepted_outstanding_operations,
        accepted_before_drop,
        "repost must not reach the provider while ready work is withheld"
    );

    ready_control.release();
    second.await.unwrap().unwrap();
    let message = server.recv().await.unwrap();
    assert_eq!(&*message, b"second");
    drop(message);
    let client = Arc::try_unwrap(client).ok().expect("single client owner");

    close_connection_pair(listener, server, client).await;
    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}

async fn run_malformed_frames(mode: CompletionMode) {
    for (frame, expected) in [
        (TestSteadyFrame::Credit(0), "zero credits"),
        (TestSteadyFrame::Credit(1), "exceeds in-flight"),
        (
            TestSteadyFrame::Hello,
            "unexpected HELLO frame during steady-state",
        ),
        (TestSteadyFrame::BadMagicData, "bad magic"),
        (TestSteadyFrame::TrailingDataByte, "trailing bytes"),
        (
            TestSteadyFrame::TruncatedDataPayload,
            "payload extends past received data",
        ),
    ] {
        let (server_engine, server_driver) = build_engine(mode, 2, Duration::from_secs(5));
        let (client_engine, client_driver) = build_engine(mode, 2, Duration::from_secs(5));
        let (listener, server, client) =
            establish_on(&server_engine, &client_engine, MessageConfig::default()).await;

        let _ = client.test_send_frame(frame).await;
        let error = tokio::time::timeout(Duration::from_secs(10), server.recv())
            .await
            .expect("malformed frame did not wake recv")
            .unwrap_err();
        assert!(
            matches!(&error, Error::ProtocolViolation(message) if message.contains(expected)),
            "unexpected malformed-frame error: {error:?}"
        );
        let server_close = server.close().await;
        assert!(
            matches!(server_close, Err(Error::ProtocolViolation(message)) if message.contains(expected))
        );
        assert_clean_close(client.close().await);
        listener.close().await.unwrap();
        shutdown_engine(server_engine, server_driver).await;
        shutdown_engine(client_engine, client_driver).await;
    }
}

async fn run_cancellation_and_disconnect(mode: CompletionMode) {
    let (server_engine, server_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let (client_engine, client_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let (listener, server, client) =
        establish_on(&server_engine, &client_engine, MessageConfig::default()).await;
    let server = Arc::new(server);
    let client = Arc::new(client);

    let ready_control = client_engine
        .test_resources()
        .unwrap()
        .pause_ready_work()
        .unwrap();
    let cancelled_client = Arc::clone(&client);
    let cancelled = tokio::spawn(async move { cancelled_client.send(b"cancelled").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while client.test_pending_ready_work().unwrap() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled send was not queued");
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    ready_control.release();

    assert!(
        tokio::time::timeout(Duration::from_millis(100), server.recv())
            .await
            .is_err(),
        "cancelling before engine posting must not deliver a message"
    );
    client.send(b"survives").await.unwrap();
    let message = server.recv().await.unwrap();
    assert_eq!(&*message, b"survives");
    drop(message);

    let waiting_server = Arc::clone(&server);
    let pending_recv = tokio::spawn(async move { waiting_server.recv().await });
    tokio::task::yield_now().await;
    assert_clean_close(client.close().await);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), pending_recv)
            .await
            .expect("disconnect did not wake recv")
            .unwrap(),
        Err(Error::TransportClosed)
    ));
    assert_clean_close(server.close().await);
    listener.close().await.unwrap();
    drop(server);
    drop(client);
    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}

async fn run_recv_cancellation_no_loss(mode: CompletionMode) {
    let (server_engine, server_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let (client_engine, client_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let (listener, server, client) =
        establish_on(&server_engine, &client_engine, MessageConfig::default()).await;

    let mut cancelled = Box::pin(server.recv());
    poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(cancelled);

    client.send(b"successor").await.unwrap();
    let successor = tokio::time::timeout(Duration::from_secs(10), server.recv())
        .await
        .expect("successor message was lost after recv cancellation")
        .unwrap();
    assert_eq!(&*successor, b"successor");
    drop(successor);

    close_connection_pair(listener, server, client).await;
    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}

async fn run_fairness_and_independent_close(mode: CompletionMode) {
    let (server_engine, server_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let (client_engine, client_driver) = build_engine(mode, 1, Duration::from_secs(5));
    let config = MessageConfig {
        sends: 4,
        recvs: 4,
        size: 64,
    };
    let (first_listener, first_server, first_client) =
        establish_on(&server_engine, &client_engine, config).await;
    let (second_listener, second_server, second_client) =
        establish_on(&server_engine, &client_engine, config).await;
    let first_server = Arc::new(first_server);
    let first_client = Arc::new(first_client);
    let second_server = Arc::new(second_server);
    let second_client = Arc::new(second_client);

    let hot_sender = {
        let client = Arc::clone(&first_client);
        tokio::spawn(async move {
            for sequence in 0..128u32 {
                client.send(&sequence.to_le_bytes()).await.unwrap();
            }
        })
    };
    let hot_receiver = {
        let server = Arc::clone(&first_server);
        tokio::spawn(async move {
            for _ in 0..128 {
                drop(server.recv().await.unwrap());
            }
        })
    };
    tokio::task::yield_now().await;

    tokio::time::timeout(Duration::from_secs(10), async {
        let send = second_client.send(b"fair");
        let receive = second_server.recv();
        let (send, receive) = tokio::join!(send, receive);
        send.unwrap();
        let receive = receive.unwrap();
        assert_eq!(&*receive, b"fair");
        drop(receive);
    })
    .await
    .expect("hot connection starved another ready message connection");
    hot_sender.await.unwrap();
    hot_receiver.await.unwrap();

    let _ = first_client
        .test_send_frame(TestSteadyFrame::Credit(0))
        .await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), first_server.recv())
            .await
            .expect("malformed frame did not fail its connection"),
        Err(Error::ProtocolViolation(message)) if message.contains("zero credits")
    ));
    assert!(matches!(
        first_server.close().await,
        Err(Error::ProtocolViolation(message)) if message.contains("zero credits")
    ));
    assert_clean_close(first_client.close().await);
    first_listener.close().await.unwrap();

    second_client.send(b"independent").await.unwrap();
    let independent = second_server.recv().await.unwrap();
    assert_eq!(&*independent, b"independent");
    drop(independent);

    let second_client = Arc::try_unwrap(second_client)
        .ok()
        .expect("single second-client owner");
    drop(second_client);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), second_server.recv())
            .await
            .expect("frontend drop did not wake peer recv"),
        Err(Error::TransportClosed)
    ));
    assert_clean_close(second_server.close().await);
    second_listener.close().await.unwrap();
    drop(first_server);
    drop(first_client);
    drop(second_server);
    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
}

async fn run_cancelled_send_quarantine(mode: CompletionMode) {
    let recorder = DestructionRecorder::arm(512);
    let (server_engine, server_driver) = build_engine(mode, 1, Duration::from_millis(100));
    let (client_engine, client_driver) = build_engine(mode, 1, Duration::from_millis(100));
    let (listener, server, client) =
        establish_on(&server_engine, &client_engine, MessageConfig::default()).await;
    let client = Arc::new(client);
    let connection = client.test_connection().unwrap();
    let suppression = client_engine
        .test_resources()
        .unwrap()
        .suppress_next_connection_cqe_with_opcode(&connection, WcOpcode::Send)
        .unwrap();
    let sending_client = Arc::clone(&client);
    let send = tokio::spawn(async move { sending_client.send(b"quarantine").await });
    tokio::time::timeout(Duration::from_secs(10), suppression.wait_observed())
        .await
        .expect("message DATA send CQE was not observed")
        .unwrap();
    send.abort();
    assert!(send.await.unwrap_err().is_cancelled());

    let close = tokio::time::timeout(Duration::from_secs(2), client.close())
        .await
        .expect("message close did not reach its drain deadline");
    assert!(matches!(
        close,
        Err(Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));
    let diagnostics = client_engine.diagnostics();
    assert_eq!(diagnostics.quarantined_operations, 1);
    assert_eq!(diagnostics.retained_cq_credits, 1);
    let before_release = recorder.snapshot();
    let mr_deregistrations = before_release
        .iter()
        .filter(|event| event.kind == DestructionKind::MemoryRegion)
        .count();

    suppression.release().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let diagnostics = client_engine.diagnostics();
            if diagnostics.live_connection_reservations == 0
                && diagnostics.quarantined_operations == 0
                && diagnostics.retained_cq_credits == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("released message CQE did not recover quarantine");
    assert!(matches!(
        client.close().await,
        Err(Error::ConnectionQuarantined {
            outstanding_operations: 1,
            cq_debt: 1
        })
    ));
    drop(connection);
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if recorder
                .snapshot()
                .iter()
                .filter(|event| event.kind == DestructionKind::MemoryRegion)
                .count()
                > mr_deregistrations
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the cancelled DATA MR was not released after its exact CQE");

    assert_clean_close(server.close().await);
    listener.close().await.unwrap();
    shutdown_engine(server_engine, server_driver).await;
    shutdown_engine(client_engine, client_driver).await;
    let _ = recorder.take();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn data_boundaries_registered_reuse_and_negotiated_credits() {
    if !has_software_rdma() {
        return;
    }
    run_boundaries_and_reuse(CompletionMode::Readiness).await;
    run_boundaries_and_reuse(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn received_message_drop_reposts_and_returns_credit_only_in_engine_work() {
    if !has_software_rdma() {
        return;
    }
    run_held_repost(CompletionMode::Readiness).await;
    run_held_repost(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn malformed_and_duplicate_control_frames_fail_connection_locally() {
    if !has_software_rdma() {
        return;
    }
    run_malformed_frames(CompletionMode::Readiness).await;
    run_malformed_frames(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn queued_send_cancellation_and_disconnect_wake_observers() {
    if !has_software_rdma() {
        return;
    }
    run_cancellation_and_disconnect(CompletionMode::Readiness).await;
    run_cancellation_and_disconnect(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn cancelled_recv_does_not_consume_successor_message_in_both_modes() {
    if !has_software_rdma() {
        return;
    }
    run_recv_cancellation_no_loss(CompletionMode::Readiness).await;
    run_recv_cancellation_no_loss(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn hot_message_work_rotates_and_connection_close_is_independent() {
    if !has_software_rdma() {
        return;
    }
    run_fairness_and_independent_close(CompletionMode::Readiness).await;
    run_fairness_and_independent_close(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn cancelled_data_send_retains_mr_until_exact_cqe_and_memoizes_quarantine() {
    if !has_software_rdma() {
        return;
    }
    run_cancelled_send_quarantine(CompletionMode::Readiness).await;
    run_cancelled_send_quarantine(CompletionMode::Polling).await;
}
