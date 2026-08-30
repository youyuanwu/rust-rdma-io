//! Integration tests for the v2 message-oriented Send/Recv transport.
//!
//! Tests cover both readiness (fd/channel-based) and polling completion modes,
//! verifying identical behavior per SC-008. Tests use deterministic
//! synchronization (channels, barriers, bounded timeouts) — no wall-clock
//! sleeps for synchronization.

use rdma_io::v2::error::TransportErrorKind;
use rdma_io::v2::*;
use rdma_io_tests::test_helpers::*;
use std::time::Duration;

macro_rules! require_software_rdma {
    () => {
        if !rdma_io_tests::test_helpers::has_software_rdma() {
            tracing::warn!("SKIPPED: no software RDMA device (rxe/siw)");
            return;
        }
    };
}

/// Connect a client/server transport pair with the given parameters.
async fn make_transport_pair(
    mode: CompletionMode,
    recv_bufs: usize,
    send_bufs: usize,
    buf_size: usize,
) -> (MessageTransport, MessageTransport) {
    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(recv_bufs)
        .send_buffers(send_bufs)
        .buffer_size(buf_size)
        .completion_mode(mode);

    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(recv_bufs)
        .send_buffers(send_bufs)
        .buffer_size(buf_size)
        .completion_mode(mode);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        tokio::spawn(driver);
        transport
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        tokio::spawn(driver);
        transport
    });

    let (server, client) = tokio::join!(server_task, client_task);
    (client.unwrap(), server.unwrap())
}

// ============================================================
// Readiness mode tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_single_message_readiness() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 8, 4, 4096).await;
    let msg = b"hello from client";
    client.send(msg).await.unwrap();

    let received = server.recv().await.unwrap();
    assert_eq!(received.as_ref(), msg);
    assert_eq!(received.len(), msg.len());

    // Reverse direction
    let reply = b"hello from server";
    server.send(reply).await.unwrap();
    let received = client.recv().await.unwrap();
    assert_eq!(received.as_ref(), reply);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_multiple_messages_readiness() {
    require_software_rdma!();

    tokio::time::timeout(Duration::from_secs(10), async {
        let (client, server) = make_transport_pair(CompletionMode::Readiness, 8, 4, 4096).await;
        for i in 0..10u32 {
            let msg = format!("message {i}");
            client.send(msg.as_bytes()).await.unwrap();

            let received = server.recv().await.unwrap();
            assert_eq!(received.as_ref(), msg.as_bytes());
        }
    })
    .await
    .expect("readiness message sequence timed out");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_oversize_rejected() {
    require_software_rdma!();

    let (client, _server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 64).await;
    let too_large = vec![0u8; 65];
    let result = client.send(&too_large).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        Error::MessageTooLarge { size, capacity } => {
            assert_eq!(size, 65);
            assert_eq!(capacity, 64);
        }
        other => panic!("expected MessageTooLarge, got {other}"),
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_max_size_message() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;
    let exact = vec![42u8; 256];
    client.send(&exact).await.unwrap();

    let received = server.recv().await.unwrap();
    assert_eq!(received.as_ref(), &exact[..]);
    assert_eq!(received.len(), 256);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_zero_length_message() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;
    client.send(b"").await.unwrap();

    let received = server.recv().await.unwrap();
    assert!(received.is_empty());
    assert_eq!(received.len(), 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_buffer_reuse_beyond_pool() {
    require_software_rdma!();

    // 4 recv buffers, send 12 messages (3× pool depth)
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;
    for i in 0..12u32 {
        let msg = format!("msg-{i}");
        client.send(msg.as_bytes()).await.unwrap();

        let received = server.recv().await.unwrap();
        assert_eq!(received.as_ref(), msg.as_bytes());
        // ReceivedMessage drops here → MR returned for reposting + credit returned
    }
}

// ============================================================
// Polling mode tests (SC-008: both modes produce identical behavior)
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_single_message_polling() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 8, 4, 4096).await;
    let msg = b"hello polling";
    client.send(msg).await.unwrap();

    let received = server.recv().await.unwrap();
    assert_eq!(received.as_ref(), msg);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_multiple_messages_polling() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 8, 4, 4096).await;
    for i in 0..10u32 {
        let msg = format!("polling-{i}");
        client.send(msg.as_bytes()).await.unwrap();

        let received = server.recv().await.unwrap();
        assert_eq!(received.as_ref(), msg.as_bytes());
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_oversize_rejected_polling() {
    require_software_rdma!();

    let (client, _server) = make_transport_pair(CompletionMode::Polling, 4, 4, 64).await;
    let too_large = vec![0u8; 65];
    let result = client.send(&too_large).await;
    assert!(matches!(result.unwrap_err(), Error::MessageTooLarge { .. }));
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_max_size_message_polling() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 4, 4, 256).await;
    let exact = vec![42u8; 256];
    client.send(&exact).await.unwrap();
    let received = server.recv().await.unwrap();
    assert_eq!(received.as_ref(), &exact[..]);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_zero_length_message_polling() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 4, 4, 256).await;
    client.send(b"").await.unwrap();
    let received = server.recv().await.unwrap();
    assert!(received.is_empty());
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_buffer_reuse_beyond_pool_polling() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 4, 4, 256).await;
    for i in 0..12u32 {
        let msg = format!("msg-{i}");
        client.send(msg.as_bytes()).await.unwrap();
        let received = server.recv().await.unwrap();
        assert_eq!(received.as_ref(), msg.as_bytes());
    }
}

// ============================================================
// Cancellation tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_recv_cancel_no_message_loss() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // Cancel a recv() that hasn't received anything (timeout)
    let recv_result = tokio::time::timeout(Duration::from_millis(50), server.recv()).await;
    assert!(recv_result.is_err()); // timed out, not a message loss

    // Now send a message
    client.send(b"after cancel").await.unwrap();

    // Next recv() should get it — the cancelled recv consumed nothing
    let msg = tokio::time::timeout(Duration::from_secs(5), server.recv())
        .await
        .expect("recv should not hang")
        .unwrap();
    assert_eq!(msg.as_ref(), b"after cancel");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_send_cancel_buffer_recovered() {
    require_software_rdma!();

    // 2 send buffers — send and recv multiple rounds to prove buffers are
    // returned to the pool correctly.
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 2, 256).await;

    // Send 3 messages with only 2 send buffers — each must be returned
    // to the pool before the next can proceed.
    for i in 0..3u32 {
        let msg = format!("cancel-recovery-{i}");
        client.send(msg.as_bytes()).await.unwrap();
        let m = server.recv().await.unwrap();
        assert_eq!(m.as_ref(), msg.as_bytes());
    }
}

// ============================================================
// Concurrent sends and ordering tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_concurrent_sends() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 16, 8, 256).await;
    let client = std::sync::Arc::new(client);

    let n = 8;
    let mut handles = Vec::new();
    for i in 0..n {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let msg = format!("concurrent-{i}");
            c.send(msg.as_bytes()).await.unwrap();
        }));
    }

    // Wait for all sends to complete
    for h in handles {
        h.await.unwrap();
    }

    // Receive all — each message delivered exactly once
    let mut received_msgs = Vec::new();
    for _ in 0..n {
        let msg = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("recv should not hang")
            .unwrap();
        received_msgs.push(String::from_utf8(msg.as_ref().to_vec()).unwrap());
    }

    received_msgs.sort();
    let mut expected: Vec<String> = (0..n).map(|i| format!("concurrent-{i}")).collect();
    expected.sort();
    assert_eq!(received_msgs, expected);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_concurrent_receivers() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 8, 8, 256).await;
    let server = std::sync::Arc::new(server);

    let n = 4usize;

    // Spawn N receiver tasks
    let mut recv_handles = Vec::new();
    for _ in 0..n {
        let s = server.clone();
        recv_handles.push(tokio::spawn(async move {
            let msg = tokio::time::timeout(Duration::from_secs(5), s.recv())
                .await
                .expect("recv should not hang")
                .unwrap();
            String::from_utf8(msg.as_ref().to_vec()).unwrap()
        }));
    }

    // Send N messages
    for i in 0..n {
        let msg = format!("to-receiver-{i}");
        client.send(msg.as_bytes()).await.unwrap();
    }

    // Each message delivered to exactly one receiver
    let mut results = Vec::new();
    for h in recv_handles {
        results.push(h.await.unwrap());
    }
    results.sort();
    assert_eq!(results.len(), n);
    // All unique
    let unique: std::collections::HashSet<_> = results.iter().collect();
    assert_eq!(unique.len(), n);
}

// ============================================================
// Backpressure / credit tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_send_backpressure() {
    require_software_rdma!();

    // 2 send buffers: saturate, receive, then send again
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 2, 256).await;
    let client = std::sync::Arc::new(client);

    // Send 2 messages (saturating the pool)
    let c1 = client.clone();
    let h1 = tokio::spawn(async move { c1.send(b"sat1").await.unwrap() });
    let c2 = client.clone();
    let h2 = tokio::spawn(async move { c2.send(b"sat2").await.unwrap() });

    h1.await.unwrap();
    h2.await.unwrap();

    // Receive both to free up buffers (and return credits)
    let m1 = server.recv().await.unwrap();
    let m2 = server.recv().await.unwrap();
    drop(m1);
    drop(m2);

    // Give a moment for credits to propagate
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Now send again — should work (buffers recovered + credits returned)
    let send_result = tokio::time::timeout(Duration::from_secs(5), client.send(b"after-bp"))
        .await
        .expect("send should not hang after credit return");
    send_result.unwrap();

    let m3 = server.recv().await.unwrap();
    assert_eq!(m3.as_ref(), b"after-bp");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_credit_flow_control() {
    require_software_rdma!();

    // 3 data recv buffers on the server → client gets 3 credits.
    // Send 3 messages, hold all, then the 4th send should block on credits.
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 3, 4, 256).await;
    let client = std::sync::Arc::new(client);

    // Send 3 messages (consuming all credits)
    for i in 0..3u32 {
        client.send(format!("credit-{i}").as_bytes()).await.unwrap();
    }

    // Hold all 3 messages (don't drop)
    let m1 = server.recv().await.unwrap();
    let m2 = server.recv().await.unwrap();
    let m3 = server.recv().await.unwrap();

    // 4th send should timeout (no credits available)
    let c4 = client.clone();
    let fourth = tokio::time::timeout(Duration::from_millis(200), async move {
        c4.send(b"blocked").await
    })
    .await;
    assert!(fourth.is_err(), "4th send should block on credits");

    // Drop one message → repost + credit return → 4th send unblocks
    drop(m1);

    let c4 = client.clone();
    let result = tokio::time::timeout(Duration::from_secs(5), async move {
        c4.send(b"unblocked").await
    })
    .await
    .expect("send should complete after credit return");
    result.unwrap();

    let received = server.recv().await.unwrap();
    assert_eq!(received.as_ref(), b"unblocked");

    drop(m2);
    drop(m3);
}

// ============================================================
// Disconnect tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_disconnect_wakes_pending_recv() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // Verify connection works
    client.send(b"ping").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"ping");
    drop(msg);

    // Spawn a pending recv on the server
    let server = std::sync::Arc::new(server);
    let s2 = server.clone();
    let recv_task = tokio::spawn(async move { s2.recv().await });

    // Give recv time to park
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Drop client → triggers disconnect
    drop(client);

    // Server's pending recv should unblock with TransportClosed
    let result = tokio::time::timeout(Duration::from_secs(10), recv_task)
        .await
        .expect("recv should not hang after disconnect");
    let recv_result = result.unwrap();
    assert!(
        recv_result.is_err(),
        "recv should fail after peer disconnect"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_disconnect_wakes_pending_send() {
    require_software_rdma!();

    // 2 recv buffers → 2 credits. Send 2 to exhaust credits, then hold messages.
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 2, 4, 256).await;

    client.send(b"fill1").await.unwrap();
    client.send(b"fill2").await.unwrap();

    // Hold messages to prevent credit return
    let _m1 = server.recv().await.unwrap();
    let _m2 = server.recv().await.unwrap();

    // Next send blocks on credits
    let client = std::sync::Arc::new(client);
    let c2 = client.clone();
    let send_task = tokio::spawn(async move { c2.send(b"blocked-send").await });

    tokio::task::yield_now().await;

    // Drop server → triggers disconnect
    drop(server);
    drop(_m1);
    drop(_m2);

    // Client's pending send should unblock with error
    let result = tokio::time::timeout(Duration::from_secs(10), send_task)
        .await
        .expect("send should not hang after disconnect");
    let send_result = result.unwrap();
    assert!(
        send_result.is_err(),
        "send should fail after peer disconnect"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_disconnect_wakes_pending_recv_polling() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 4, 4, 256).await;

    client.send(b"ping").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"ping");
    drop(msg);

    let server = std::sync::Arc::new(server);
    let s2 = server.clone();
    let recv_task = tokio::spawn(async move { s2.recv().await });

    tokio::task::yield_now().await;
    drop(client);

    let result = tokio::time::timeout(Duration::from_secs(10), recv_task)
        .await
        .expect("recv should not hang after disconnect (polling mode)");
    let recv_result = result.unwrap();
    assert!(
        recv_result.is_err(),
        "recv should fail after peer disconnect"
    );
}

// ============================================================
// Shutdown tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_drop_no_hang() {
    require_software_rdma!();

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;
        client.send(b"test").await.unwrap();
        let _ = server.recv().await.unwrap();
        drop(client);
        drop(server);
    })
    .await;
    assert!(result.is_ok(), "transport drop should not hang");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shutdown_wakes_recv() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    client.send(b"before-shutdown").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"before-shutdown");
    drop(msg);

    server.close().await;
    drop(client);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shutdown_wakes_pending_recv() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    client.send(b"test").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"test");
    drop(msg);

    let conn_handles: Vec<_> = server.driver_handles().to_vec();
    let server = std::sync::Arc::new(server);
    let s2 = server.clone();
    let recv_task = tokio::spawn(async move { s2.recv().await });

    tokio::task::yield_now().await;

    for handle in &conn_handles {
        handle.flush_and_shutdown();
    }

    let result = tokio::time::timeout(Duration::from_secs(10), recv_task)
        .await
        .expect("recv should not hang forever");
    let recv_result = result.unwrap();
    assert!(recv_result.is_err(), "recv should fail after shutdown");

    drop(server);
    drop(client);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_close_no_hang() {
    require_software_rdma!();

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;
        client.send(b"before close").await.unwrap();
        let _ = server.recv().await.unwrap();
        client.close().await;
        server.close().await;
    })
    .await;
    assert!(result.is_ok(), "close should not hang");
}

// ============================================================
// Polling mode shutdown
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_drop_no_hang_polling() {
    require_software_rdma!();

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let (client, server) = make_transport_pair(CompletionMode::Polling, 4, 4, 256).await;
        client.send(b"test").await.unwrap();
        let _ = server.recv().await.unwrap();
        drop(client);
        drop(server);
    })
    .await;
    assert!(result.is_ok(), "polling transport drop should not hang");
}

// ============================================================
// Shared CQ sole-ownership regression
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shared_cq_single_driver() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    assert_eq!(
        client.driver_handles().len(),
        1,
        "shared CQ should have exactly one driver handle"
    );
    assert_eq!(
        server.driver_handles().len(),
        1,
        "shared CQ should have exactly one driver handle"
    );

    client.send(b"shared-cq-test").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"shared-cq-test");

    server.send(b"reverse").await.unwrap();
    let msg = client.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"reverse");
}

// ============================================================
// Registry / reclaim stress
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_inflight_registry_reclaim() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 8, 4, 256).await;

    for i in 0..50u32 {
        let msg = format!("reclaim-{i}");
        client.send(msg.as_bytes()).await.unwrap();
        let received = server.recv().await.unwrap();
        assert_eq!(received.as_ref(), msg.as_bytes());
    }
}

// ============================================================
// Credit accounting invariant tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_credit_never_exceeds_capacity() {
    require_software_rdma!();

    // Use 4 recv buffers → 4 credits. Do many round trips.
    // Credits should never exceed 4 (the announced capacity).
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // 20 round trips — each time the credit is consumed and returned.
    for i in 0..20u32 {
        let msg = format!("credit-cycle-{i}");
        client.send(msg.as_bytes()).await.unwrap();
        let received = server.recv().await.unwrap();
        assert_eq!(received.as_ref(), msg.as_bytes());
        // ReceivedMessage drops here → credit returned
    }
}

// ============================================================
// Explicit driver spawning lifecycle tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_no_progress_without_driver_poll() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move { server_builder.accept(&listener).await.unwrap() });
    let client_task =
        tokio::spawn(async move { client_builder.connect(listen_addr).await.unwrap() });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver) = client.unwrap();
    let (_server_transport, server_driver) = server.unwrap();

    // Without drivers spawned, ready() should not complete
    let result = tokio::time::timeout(Duration::from_millis(200), client_transport.ready()).await;
    assert!(
        result.is_err(),
        "ready() should timeout without driver running"
    );

    // Drop drivers triggers failure state via Drop guard
    drop(client_driver);
    drop(server_driver);

    // Now ready() should return error
    let result = client_transport.ready().await;
    assert!(result.is_err(), "ready() should error after driver dropped");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_readiness_completes_after_both_drivers() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        client.ready().await.unwrap();
        server.ready().await.unwrap();
    })
    .await;
    assert!(
        result.is_ok(),
        "readiness should complete after both drivers spawned"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_drop_unspawned_driver_fails_frontend() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move { server_builder.accept(&listener).await.unwrap() });
    let client_task =
        tokio::spawn(async move { client_builder.connect(listen_addr).await.unwrap() });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver) = client.unwrap();
    let (_server_transport, server_driver) = server.unwrap();

    // Drop drivers without spawning
    drop(client_driver);
    drop(server_driver);

    // Frontend operations should return error (TransportFailed, not TransportClosed)
    let ready_err = client_transport.ready().await.unwrap_err();
    assert!(
        matches!(ready_err, Error::TransportFailed(_)),
        "ready() should return TransportFailed after driver dropped, got {ready_err}"
    );

    let send_err = client_transport.send(b"hello").await.unwrap_err();
    assert!(
        matches!(send_err, Error::TransportFailed(_)),
        "send() should return TransportFailed after driver dropped, got {send_err}"
    );

    let recv_err = client_transport.recv().await.unwrap_err();
    assert!(
        matches!(recv_err, Error::TransportFailed(_)),
        "recv() should return TransportFailed after driver dropped, got {recv_err}"
    );

    // error() should report DriverAborted
    let err = client_transport
        .error()
        .expect("error() should return Some after driver dropped");
    assert_eq!(
        *err.kind(),
        TransportErrorKind::DriverAborted,
        "error kind should be DriverAborted"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_abort_driver_task_fails_frontend() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    client.send(b"before-abort").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"before-abort");
    drop(msg);

    // Shut down the server's driver handles to simulate driver failure
    for handle in server.driver_handles() {
        handle.flush_and_shutdown();
    }

    // Server's recv should eventually fail
    let result = tokio::time::timeout(Duration::from_secs(10), server.recv()).await;
    assert!(result.is_ok(), "recv should not hang after driver abort");
    assert!(
        result.unwrap().is_err(),
        "recv should fail after driver shutdown"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_frontend_close_exits_driver() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver_handle) = client.unwrap();
    let (server_transport, server_driver_handle) = server.unwrap();

    client_transport.send(b"test").await.unwrap();
    let _ = server_transport.recv().await.unwrap();

    client_transport.close().await;

    let result = tokio::time::timeout(Duration::from_secs(10), client_driver_handle).await;
    assert!(result.is_ok(), "driver should exit after close()");

    drop(server_transport);
    let _ = server_driver_handle.await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_frontend_drop_exits_driver() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver_handle) = client.unwrap();
    let (_server_transport, _server_driver_handle) = server.unwrap();

    // Drop frontend — driver should detect and shut down
    drop(client_transport);

    let result = tokio::time::timeout(Duration::from_secs(10), client_driver_handle).await;
    assert!(result.is_ok(), "driver should exit after frontend drop");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_close_unspawned_driver_no_hang() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move { server_builder.accept(&listener).await.unwrap() });
    let client_task =
        tokio::spawn(async move { client_builder.connect(listen_addr).await.unwrap() });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver) = client.unwrap();
    let (_server_transport, _server_driver) = server.unwrap();

    // Drop driver first, then close()
    drop(client_driver);

    let result = tokio::time::timeout(Duration::from_secs(5), client_transport.close()).await;
    assert!(
        result.is_ok(),
        "close() should return immediately with dropped driver"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_one_task_per_endpoint_separate_cq() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness)
        .separate_cqs(true);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness)
        .separate_cqs(true);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        tokio::spawn(driver);
        transport
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        tokio::spawn(driver);
        transport
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let client = client.unwrap();
    let server = server.unwrap();

    // Separate CQs: two handles but still one driver future/task
    assert_eq!(
        client.driver_handles().len(),
        2,
        "separate CQ: two driver handles"
    );
    assert_eq!(
        server.driver_handles().len(),
        2,
        "separate CQ: two driver handles"
    );

    client.send(b"separate-cq").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"separate-cq");

    server.send(b"reverse-separate").await.unwrap();
    let msg = client.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"reverse-separate");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_readiness_mode_explicit_spawn() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;
    client.ready().await.unwrap();
    server.ready().await.unwrap();
    client.send(b"readiness-mode").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"readiness-mode");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_polling_mode_explicit_spawn() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Polling, 4, 4, 256).await;
    client.ready().await.unwrap();
    server.ready().await.unwrap();
    client.send(b"polling-mode").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"polling-mode");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_one_task_per_endpoint_shared_cq() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    assert_eq!(
        client.driver_handles().len(),
        1,
        "shared CQ: one driver handle per endpoint"
    );
    assert_eq!(
        server.driver_handles().len(),
        1,
        "shared CQ: one driver handle per endpoint"
    );

    client.send(b"shared-cq-structural").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"shared-cq-structural");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_driver_error_propagates() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver_handle) = client.unwrap();
    let (server_transport, server_driver_handle) = server.unwrap();

    // Exchange a message to confirm things work
    client_transport.send(b"pre-error").await.unwrap();
    let _ = server_transport.recv().await.unwrap();

    // Abort the server's driver task (a real tokio abort)
    server_driver_handle.abort();

    // The driver task should complete as cancelled
    let driver_result = tokio::time::timeout(Duration::from_secs(10), server_driver_handle).await;
    assert!(driver_result.is_ok(), "driver should exit after abort");

    // Frontend should observe the error via error()
    // Give a moment for the Drop guard to fire
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let err = server_transport.error();
    assert!(
        err.is_some(),
        "error() should return Some after driver abort"
    );
    assert_eq!(
        *err.unwrap().kind(),
        TransportErrorKind::DriverAborted,
        "abort should produce DriverAborted error"
    );

    // Frontend recv should fail with TransportFailed
    let recv_result = server_transport.recv().await;
    assert!(
        recv_result.is_err(),
        "frontend recv should fail after driver error"
    );

    drop(client_transport);
    let _ = client_driver_handle.await;
}

// ============================================================
// Error observation tests (FR-016)
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_error_observation_clean_close_no_error() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver_handle) = client.unwrap();
    let (server_transport, server_driver_handle) = server.unwrap();

    client_transport.send(b"test").await.unwrap();
    let _ = server_transport.recv().await.unwrap();

    // Clean close
    client_transport.close().await;
    let driver_result = tokio::time::timeout(Duration::from_secs(10), client_driver_handle)
        .await
        .expect("driver should exit after close");
    let inner = driver_result.expect("driver should not panic");
    assert!(inner.is_ok(), "driver should return Ok on clean close");

    // error() should return None for clean close
    assert!(
        client_transport.error().is_none(),
        "error() should be None after clean close"
    );

    drop(server_transport);
    let _ = server_driver_handle.await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_error_observation_driver_drop_unspawned() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move { server_builder.accept(&listener).await.unwrap() });
    let client_task =
        tokio::spawn(async move { client_builder.connect(listen_addr).await.unwrap() });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver) = client.unwrap();
    let (_server_transport, server_driver) = server.unwrap();

    // Drop drivers without spawning
    drop(client_driver);
    drop(server_driver);

    // error() should report DriverAborted for dropped-before-spawn
    let err = client_transport
        .error()
        .expect("error() should be Some after driver drop");
    assert_eq!(
        *err.kind(),
        TransportErrorKind::DriverAborted,
        "dropped unspawned driver should be DriverAborted"
    );
    assert!(
        err.message().contains("driver"),
        "error message should mention driver: {}",
        err.message()
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_error_observation_peer_disconnect_state() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // Verify connection works
    client.send(b"ping").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"ping");
    drop(msg);

    // Drop client — triggers peer disconnect
    drop(client);

    // Wait for server's recv to fail (peer disconnect detected)
    let result = tokio::time::timeout(Duration::from_secs(10), server.recv()).await;
    assert!(result.is_ok(), "recv should not hang after disconnect");
    assert!(
        result.unwrap().is_err(),
        "recv should fail after peer disconnect"
    );

    // Peer disconnect from clean close is not a driver error
    // (the peer's driver ran close() successfully)
    // error() may be None (clean peer-initiated shutdown)
    // This is expected — peer disconnect ≠ local driver failure
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_error_and_driver_result_consistent() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);

    let server_task = tokio::spawn(async move {
        let (transport, driver) = server_builder.accept(&listener).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });
    let client_task = tokio::spawn(async move {
        let (transport, driver) = client_builder.connect(listen_addr).await.unwrap();
        let handle = tokio::spawn(driver);
        (transport, handle)
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let (client_transport, client_driver_handle) = client.unwrap();
    let (server_transport, server_driver_handle) = server.unwrap();

    client_transport.send(b"test").await.unwrap();
    let _ = server_transport.recv().await.unwrap();

    // Abort server driver
    server_driver_handle.abort();

    let driver_result = tokio::time::timeout(Duration::from_secs(10), server_driver_handle)
        .await
        .expect("driver should exit");

    // Give Drop guard time to fire
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Both observation channels should be consistent:
    // - Driver JoinHandle: cancelled (or error)
    // - Frontend error(): DriverAborted
    let frontend_err = server_transport
        .error()
        .expect("error() should be set after abort");
    assert_eq!(*frontend_err.kind(), TransportErrorKind::DriverAborted);

    // The JoinHandle was cancelled so driver_result is JoinError(Cancelled)
    assert!(driver_result.is_err(), "aborted task should be JoinError");

    drop(client_transport);
    let _ = client_driver_handle.await;
}
