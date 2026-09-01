use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use futures_util::future::join_all;
use rdma_io::v2::test_support::TestSteadyFrame;
use rdma_io::v2::{
    AccessIntent, CompletionMode, Error, MessageTransport, MessageTransportBuilder, RdmaConnection,
    RdmaEngine, RdmaEngineBuilder, RdmaListenerConfig,
};
use rdma_io::wc::WcOpcode;
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

fn software_device_name() -> Option<String> {
    let list = rdma_io::cm::RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn message_builder() -> MessageTransportBuilder {
    MessageTransportBuilder::new()
        .send_buffers(4)
        .recv_buffers(4)
        .buffer_size(128)
}

fn assert_clean(result: rdma_io::v2::Result<()>) {
    assert!(
        matches!(result, Ok(()) | Err(Error::TransportClosed)),
        "unexpected close result: {result:?}"
    );
}

fn assert_protocol_pair_close(result: rdma_io::v2::Result<()>) {
    assert!(
        matches!(
            &result,
            Err(Error::ProtocolViolation(message)) if message.contains("zero credits")
        ) || matches!(&result, Ok(()) | Err(Error::TransportClosed)),
        "expected zero-credit protocol failure or its peer close, got: {result:?}"
    );
}

fn assert_local_protocol_violation(result: rdma_io::v2::Result<()>) {
    assert!(
        matches!(
            &result,
            Err(Error::ProtocolViolation(message)) if message.contains("zero credits")
        ),
        "expected zero-credit protocol violation, got: {result:?}"
    );
}

async fn establish_message_pair(
    engine: &RdmaEngine,
    listener: &rdma_io::v2::RdmaListener,
    address: std::net::SocketAddr,
) -> (Arc<MessageTransport>, Arc<MessageTransport>) {
    let (server, client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            message_builder().accept_on(listener),
            message_builder().connect_on(engine, address)
        )
    })
    .await
    .expect("message establishment timed out");
    let server = Arc::new(server.unwrap());
    let client = Arc::new(client.unwrap());
    let (server_ready, client_ready) = tokio::join!(server.ready(), client.ready());
    server_ready.unwrap();
    client_ready.unwrap();
    (server, client)
}

async fn bidirectional_low_level(server: &RdmaConnection, client: &RdmaConnection) {
    let recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut send = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    send.as_mut_slice()[..4].copy_from_slice(b"c2s!");
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(server.recv(recv, None), client.send(send, Some((0, 4))));
    recv_result.unwrap();
    send_result.unwrap();
    assert_eq!(&recv.unwrap().as_slice()[..4], b"c2s!");
    assert!(send.is_some());

    let recv = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut send = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    send.as_mut_slice()[..4].copy_from_slice(b"s2c!");
    let ((recv_result, recv), (send_result, send)) =
        tokio::join!(client.recv(recv, None), server.send(send, Some((0, 4))));
    recv_result.unwrap();
    send_result.unwrap();
    assert_eq!(&recv.unwrap().as_slice()[..4], b"s2c!");
    assert!(send.is_some());
}

async fn assert_out_of_order_isolation(pairs: &[(Arc<MessageTransport>, Arc<MessageTransport>)]) {
    let order = [7_usize, 0, 5, 2, 6, 1, 4, 3];
    join_all(order.into_iter().map(|index| {
        let client = Arc::clone(&pairs[index].1);
        async move {
            client
                .send(format!("pair-{index}").as_bytes())
                .await
                .unwrap();
        }
    }))
    .await;
    for index in order.into_iter().rev() {
        let message = pairs[index].0.recv().await.unwrap();
        assert_eq!(&*message, format!("pair-{index}").as_bytes());
        drop(message);
    }
}

async fn assert_hot_connection_fairness(pairs: &[(Arc<MessageTransport>, Arc<MessageTransport>)]) {
    let stop = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let hot = {
        let server = Arc::clone(&pairs[0].0);
        let client = Arc::clone(&pairs[0].1);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut sequence = 0_u32;
            let mut started_tx = Some(started_tx);
            while !stop.load(Ordering::Acquire) {
                let payload = sequence.to_le_bytes();
                client.send(&payload).await.unwrap();
                let request = server.recv().await.unwrap();
                assert_eq!(&*request, &payload);
                drop(request);
                server.send(&payload).await.unwrap();
                let response = client.recv().await.unwrap();
                assert_eq!(&*response, &payload);
                drop(response);
                sequence = sequence.wrapping_add(1);
                if let Some(started_tx) = started_tx.take() {
                    let _ = started_tx.send(());
                }
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(10), started_rx)
        .await
        .expect("hot connection did not start")
        .expect("hot connection ended before starting");

    let intermittent = join_all((1..8).map(|index| {
        let server = Arc::clone(&pairs[index].0);
        let client = Arc::clone(&pairs[index].1);
        async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                let client_payload = format!("client-{index}");
                let server_payload = format!("server-{index}");
                let (client_send, server_send) = tokio::join!(
                    client.send(client_payload.as_bytes()),
                    server.send(server_payload.as_bytes())
                );
                client_send.unwrap();
                server_send.unwrap();
                let (server_recv, client_recv) = tokio::join!(server.recv(), client.recv());
                let server_recv = server_recv.unwrap();
                let client_recv = client_recv.unwrap();
                assert_eq!(&*server_recv, client_payload.as_bytes());
                assert_eq!(&*client_recv, server_payload.as_bytes());
            })
            .await
            .expect("hot connection starved an intermittent connection");
        }
    }))
    .await;
    assert_eq!(intermittent.len(), 7);
    stop.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(10), hot)
        .await
        .expect("hot connection did not stop")
        .unwrap();
}

async fn assert_held_credit_and_cancellation(
    server: &Arc<MessageTransport>,
    client: &Arc<MessageTransport>,
) {
    for index in 0..4_u8 {
        client.send(&[index]).await.unwrap();
    }
    let mut held = Vec::new();
    for index in 0..4_u8 {
        let message = server.recv().await.unwrap();
        assert_eq!(&*message, &[index]);
        held.push(message);
    }

    let mut cancelled = Box::pin(client.send(b"cancelled"));
    poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(cancelled);

    drop(held.pop());
    client.send(b"successor").await.unwrap();
    let successor = server.recv().await.unwrap();
    assert_eq!(&*successor, b"successor");
    drop(successor);
    drop(held);
}

async fn assert_recv_cancellation_no_loss(
    server: &Arc<MessageTransport>,
    client: &Arc<MessageTransport>,
) {
    let mut cancelled = Box::pin(server.recv());
    poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(cancelled);
    client.send(b"recv-successor").await.unwrap();
    let successor = server.recv().await.unwrap();
    assert_eq!(&*successor, b"recv-successor");
    drop(successor);
}

async fn run_mode(mode: CompletionMode) {
    let (engine, driver) =
        RdmaEngineBuilder::new(software_device_name().expect("software RDMA device"))
            .completion_mode(mode)
            .maximum_live_connections(20)
            .maximum_inflight_operations(2_048)
            .cq_capacity(2_048)
            .cq_completion_budget(7)
            .cm_event_budget(7)
            .reclamation_budget(7)
            .ready_connection_quantum(1)
            .connection_drain_deadline(Duration::from_secs(5))
            .shutdown_deadline(Duration::from_secs(10))
            .build()
            .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver = tokio::spawn(driver);
    let listener = engine
        .listen(
            "0.0.0.0:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(16),
        )
        .await
        .unwrap();
    let address = connect_addr_for(Some(listener.local_addr().unwrap()));

    let mut pairs = Vec::new();
    for _ in 0..8 {
        pairs.push(establish_message_pair(&engine, &listener, address).await);
    }
    let (low_server, low_client) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(listener.accept(), engine.connect(address))
    })
    .await
    .expect("low-level establishment timed out");
    let low_server = low_server.unwrap();
    let low_client = low_client.unwrap();

    let shared = engine.diagnostics();
    assert_eq!(shared.live_connection_reservations, 18);
    assert_eq!(shared.establishing_connection_reservations, 0);
    assert_eq!(shared.established_connection_reservations, 18);
    assert_eq!(shared.connections().len(), 18);
    assert_eq!(shared.connections_admitted, 18);
    assert_eq!(shared.connections_opened, 18);
    assert_eq!(shared.inbound_requests_accepted, 9);
    assert_eq!(shared.shared_contexts, 1);
    assert_eq!(shared.shared_protection_domains, 1);
    assert_eq!(shared.shared_completion_queues, 1);
    assert_eq!(shared.shared_cm_event_channels, 1);
    assert_eq!(shared.shared_cm_event_fds, 1);
    assert_eq!(shared.explicit_engine_drivers, 1);
    assert_eq!(shared.library_owned_tasks, 0);
    assert_eq!(
        shared.shared_cq_notification_fds,
        usize::from(mode == CompletionMode::Readiness)
    );

    bidirectional_low_level(&low_server, &low_client).await;
    low_server.close().await.unwrap();
    low_client.close().await.unwrap();
    pairs[7].1.send(b"after-low-close").await.unwrap();
    let after_low_close = pairs[7].0.recv().await.unwrap();
    assert_eq!(&*after_low_close, b"after-low-close");
    drop(after_low_close);

    assert_out_of_order_isolation(&pairs).await;
    assert_hot_connection_fairness(&pairs).await;
    assert_held_credit_and_cancellation(&pairs[1].0, &pairs[1].1).await;
    assert_recv_cancellation_no_loss(&pairs[2].0, &pairs[2].1).await;

    let pending_peer_recv = {
        let server = Arc::clone(&pairs[3].0);
        tokio::spawn(async move { server.recv().await.map(|_| ()) })
    };
    assert_clean(pairs[3].1.close().await);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), pending_peer_recv)
            .await
            .expect("peer close did not wake recv")
            .unwrap(),
        Err(Error::TransportClosed)
    ));
    assert_clean(pairs[3].0.close().await);

    let _ = pairs[4].1.test_send_frame(TestSteadyFrame::Credit(0)).await;
    assert!(matches!(
        pairs[4].0.recv().await,
        Err(Error::ProtocolViolation(message)) if message.contains("zero credits")
    ));
    assert_local_protocol_violation(pairs[4].0.close().await);
    assert_protocol_pair_close(pairs[4].1.close().await);

    let stale_connection = pairs[5].1.test_connection().unwrap();
    let held = resources
        .suppress_next_connection_cqe_with_opcode(&stale_connection, WcOpcode::Send)
        .unwrap();
    let stale_sender = {
        let client = Arc::clone(&pairs[5].1);
        tokio::spawn(async move { client.send(b"stale-safe").await })
    };
    held.wait_observed().await.unwrap();
    let identity = held.completion().unwrap();
    let before = engine.diagnostics();
    resources
        .inject_completion(
            identity.wr_id().wrapping_add(1_u64 << 32),
            identity.qp_num(),
            identity.opcode(),
        )
        .unwrap();
    resources
        .inject_completion(u64::MAX, identity.qp_num(), identity.opcode())
        .unwrap();
    resources
        .inject_completion(identity.wr_id(), identity.qp_num() + 1, identity.opcode())
        .unwrap();
    resources
        .inject_completion(identity.wr_id(), identity.qp_num(), WcOpcode::Recv)
        .unwrap();
    let rejected = engine.diagnostics();
    assert_eq!(
        rejected.stale_operation_cqes,
        before.stale_operation_cqes + 1
    );
    assert_eq!(rejected.unknown_cqes, before.unknown_cqes + 1);
    assert_eq!(rejected.wrong_qp_num_cqes, before.wrong_qp_num_cqes + 1);
    assert_eq!(
        rejected.unexpected_opcode_cqes,
        before.unexpected_opcode_cqes + 1
    );
    held.release().unwrap();
    stale_sender.await.unwrap().unwrap();
    let stale_safe = pairs[5].0.recv().await.unwrap();
    assert_eq!(&*stale_safe, b"stale-safe");
    drop(stale_safe);
    resources
        .inject_completion(identity.wr_id(), identity.qp_num(), identity.opcode())
        .unwrap();
    assert_eq!(
        engine.diagnostics().duplicate_cqes,
        rejected.duplicate_cqes + 1
    );

    for index in [5_usize, 6, 7] {
        pairs[index].1.send(&[index as u8]).await.unwrap();
        let message = pairs[index].0.recv().await.unwrap();
        assert_eq!(&*message, &[index as u8]);
        drop(message);
    }

    let instrumentation = resources.instrumentation().unwrap();
    assert!(instrumentation.connection_selections > 0);
    assert!(instrumentation.connection_quantum_work > 0);
    assert!(instrumentation.maximum_connection_quantum_work <= 1);
    assert!(instrumentation.idle_connection_visits < instrumentation.connection_selections);
    assert!(instrumentation.operations_posted > 0);
    assert!(instrumentation.cqes_routed > 0);
    assert!(instrumentation.cqes_rejected >= 5);
    assert!(instrumentation.driver_wakeups > 0);
    if mode == CompletionMode::Polling {
        assert!(instrumentation.driver_yields > 0);
    }

    for (index, (server, client)) in pairs.iter().enumerate() {
        let (server_close, client_close) = tokio::join!(server.close(), client.close());
        if index == 4 {
            assert_local_protocol_violation(server_close);
            assert_protocol_pair_close(client_close);
        } else {
            assert_clean(server_close);
            assert_clean(client_close);
        }
    }
    listener.close().await.unwrap();
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();

    let terminal = engine.diagnostics();
    assert_eq!(terminal.live_connection_reservations, 0);
    assert_eq!(terminal.registered_operations, 0);
    assert_eq!(terminal.free_cq_credits, 2_048);
    assert_eq!(terminal.quarantined_bundles, 0);
    assert_eq!(terminal.retained_cq_credits, 0);
    assert_eq!(terminal.connections_closed, 18);
    assert_eq!(terminal.qp_destroys, 18);
    assert_eq!(terminal.retired_connection_slots, 0);
    assert_eq!(terminal.retired_operation_slots, 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 8))]
async fn eight_message_pairs_one_engine_are_isolated_fair_and_single_resource() {
    if !has_software_rdma() {
        return;
    }
    run_mode(CompletionMode::Readiness).await;
    run_mode(CompletionMode::Polling).await;
}
