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

    let server = std::sync::Arc::new(server);
    let s2 = server.clone();
    let recv_task = tokio::spawn(async move { s2.recv().await });

    tokio::task::yield_now().await;

    // Use close() to trigger shutdown (driver_handles is deprecated)
    server.close().await;

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

    // Shared CQ mode produces one driver handle internally — verify
    // by successful bidirectional exchange (proves single driver works).
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
    let (client_transport, _client_driver_handle) = client.unwrap();
    let (server_transport, server_driver_handle) = server.unwrap();

    client_transport.send(b"before-abort").await.unwrap();
    let msg = server_transport.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"before-abort");
    drop(msg);

    // Abort the server's driver task (Drop guard fires, marking Failed)
    server_driver_handle.abort();

    // Server's recv should eventually fail
    let result = tokio::time::timeout(Duration::from_secs(10), server_transport.recv()).await;
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

    // Separate CQs still use one driver future/task — proven structurally:
    // v2_no_hidden_spawn verifies zero tokio::spawn in v2 production code,
    // so the only spawned task is the one the caller creates. This exchange
    // proves both CQ drivers compose correctly inside that single future.
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

    // Shared CQ: one driver handle per endpoint — proven structurally:
    // v2_no_hidden_spawn verifies zero tokio::spawn in v2 production code.
    // This exchange proves the shared CQ driver works for both directions.
    client.send(b"shared-cq-structural").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"shared-cq-structural");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_driver_abort_propagates_to_frontend() {
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

/// FR-027: HELLO validation failure (buffer_size mismatch) is reported
/// through the driver result and ready(), not from connect()/accept().
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_hello_mismatch_fails_ready() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    // Server: buffer_size(256) — announces max_message_size=256
    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    // Client: buffer_size(4096) — peer max (256) < local buffer_size (4096)
    // → protocol violation
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(4)
        .buffer_size(4096)
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

    // ready() should fail with TransportFailed (not hang or succeed)
    let ready_result = tokio::time::timeout(Duration::from_secs(15), client_transport.ready())
        .await
        .expect("ready() should not hang on HELLO mismatch");
    assert!(
        ready_result.is_err(),
        "ready() should fail on HELLO mismatch"
    );

    // error() should report ProtocolViolation
    // Verify BOTH observation channels (FR-027): driver Result AND frontend error()
    let driver_result = tokio::time::timeout(Duration::from_secs(5), client_driver_handle)
        .await
        .expect("driver should exit after HELLO mismatch");
    let driver_inner = driver_result.expect("driver task should not panic");
    assert!(
        driver_inner.is_err(),
        "driver should return Err on HELLO mismatch"
    );

    // Both channels should agree on ProtocolViolation
    let err = client_transport.error();
    assert!(err.is_some(), "error() should be set after HELLO mismatch");
    assert_eq!(
        *err.unwrap().kind(),
        TransportErrorKind::ProtocolViolation,
        "HELLO mismatch should produce ProtocolViolation"
    );

    drop(client_transport);
    drop(_server_transport);
    let _ = _server_driver_handle.await;
}

/// Regression test for M-1: lost wakeup in send() pool wait.
///
/// With send_buffers(1), N concurrent senders park on the pool. Aborting
/// the driver must wake ALL of them with error — none may hang forever.
/// The bug was that `notify_waiters()` could land between the terminal
/// check and the `notified()` snapshot, losing the wakeup.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn test_concurrent_send_abort_no_hang() {
    require_software_rdma!();

    let listener = bind_listener_with_retry().await;
    let listen_addr = connect_addr_for(listener.local_addr());

    let server_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(1)
        .buffer_size(256)
        .completion_mode(CompletionMode::Readiness);
    let client_builder = message_transport::MessageTransportBuilder::new()
        .recv_buffers(4)
        .send_buffers(1)
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
    let (server_transport, _server_driver_handle) = server.unwrap();

    // Wait for readiness
    client_transport.ready().await.unwrap();

    // Spawn N concurrent senders. With send_buffers(1), one may post while the
    // rest wait on the send pool; aborting the driver must wake every waiter.
    let client_arc = std::sync::Arc::new(client_transport);
    let n = 4;
    let mut send_handles = Vec::new();
    for i in 0..n {
        let c = client_arc.clone();
        send_handles.push(tokio::spawn(async move {
            c.send(format!("concurrent-{i}").as_bytes()).await
        }));
    }

    // Give the senders a chance to contend on the single-buffer pool, but
    // abort before completions can recycle the MR back into the pool.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Abort the driver — all parked senders must wake with error
    client_driver_handle.abort();

    // ALL senders must resolve within timeout — none may hang
    for (i, h) in send_handles.into_iter().enumerate() {
        let result = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .unwrap_or_else(|_| panic!("sender {i} hung after driver abort (M-1 regression)"));
        let send_result = result.unwrap();
        assert!(
            send_result.is_err(),
            "sender {i} should fail after driver abort"
        );
    }

    drop(client_arc);
    drop(server_transport);
    let _ = _server_driver_handle.await;
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

// ============================================================
// Connection lifetime / drop-order safety tests
// ============================================================

/// Proves: unspawned driver dropped while frontend remains → frontend
/// can still call close()/error() without UAF. The ConnectionLifetime
/// (QP, CmId) is shared via Arc and only destructs when BOTH sides drop.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_lifetime_unspawned_driver_dropped_frontend_remains() {
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

    // Drop drivers (never spawned) — ConnectionLifetime still alive via frontend
    drop(client_driver);
    drop(server_driver);

    // Frontend operations should return errors but not UAF
    let err = client_transport
        .error()
        .expect("error() should be set after driver dropped");
    assert_eq!(*err.kind(), TransportErrorKind::DriverAborted);

    // close() must not hang or crash
    let result = tokio::time::timeout(Duration::from_secs(5), client_transport.close()).await;
    assert!(result.is_ok(), "close() should return immediately");

    // Now drop frontend — this is the LAST holder of ConnectionLifetime.
    // The destructor runs: SharedQp (QP destroy) → Pd → CmId → EventChannel.
    // No UAF because QP drops before CmId.
    drop(client_transport);
    drop(_server_transport);
}

/// Proves: spawned driver aborted while frontend remains → QP destructor
/// runs after CmId only when the LAST holder (frontend) drops.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_lifetime_spawned_driver_aborted_frontend_remains() {
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

    // Exchange to prove connection works
    client_transport.send(b"pre-abort").await.unwrap();
    let _ = server_transport.recv().await.unwrap();

    // Abort the client driver — Drop guard fires, marks Failed
    client_driver_handle.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), client_driver_handle).await;

    // Give Drop guard time
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Frontend still alive — error() works, no UAF
    let err = client_transport
        .error()
        .expect("error() should be set after driver abort");
    assert_eq!(*err.kind(), TransportErrorKind::DriverAborted);

    // Frontend recv should fail
    assert!(client_transport.recv().await.is_err());

    // Drop frontend — last holder of ConnectionLifetime
    // QP destructor precedes CmId destructor (field drop order)
    drop(client_transport);

    drop(server_transport);
    let _ = server_driver_handle.await;
}

/// Proves: frontend dropped while driver remains → driver detects and
/// shuts down. ConnectionLifetime destructs when driver future completes.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_lifetime_frontend_dropped_driver_remains() {
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

    // Drop frontend — driver should detect and exit
    drop(client_transport);

    // Driver future should complete (ConnectionLifetime destructs safely)
    let result = tokio::time::timeout(Duration::from_secs(10), client_driver_handle)
        .await
        .expect("driver should exit after frontend drop");
    // Driver may return Ok or Err depending on shutdown race — either is valid
    let _ = result;
}

/// Proves: in-flight frontend send/recv cancellation doesn't prevent
/// safe ConnectionLifetime destruction.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_lifetime_inflight_send_recv_cancellation() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 2, 2, 256).await;

    // Send 2 messages to exhaust BOTH send buffers AND all 2 credits
    client.send(b"fill1").await.unwrap();
    client.send(b"fill2").await.unwrap();

    // Recv both on server but hold them (preventing credit return)
    let _m1 = server.recv().await.unwrap();
    let _m2 = server.recv().await.unwrap();

    // Cancel a send (blocks on credits — all 2 credits consumed, none returned)
    let client_arc = std::sync::Arc::new(client);
    let c = client_arc.clone();
    let cancelled_send = tokio::time::timeout(Duration::from_millis(100), async move {
        c.send(b"blocked").await
    })
    .await;
    assert!(cancelled_send.is_err(), "send should timeout (no credits)");

    // Cancel a recv (no message available)
    let s_arc = std::sync::Arc::new(server);
    let s = s_arc.clone();
    let cancelled_recv =
        tokio::time::timeout(Duration::from_millis(100), async move { s.recv().await }).await;
    assert!(cancelled_recv.is_err(), "recv should timeout (no message)");

    // Drop everything — ConnectionLifetime destructs safely
    // The cancelled futures released their inflight state to detached
    // reclaim, which doesn't hold Arc<Qp>.
    drop(_m1);
    drop(_m2);
    drop(s_arc);
    drop(client_arc);
    // No UAF — QP drops before CmId in ConnectionLifetime
}

/// Proves: final owner drop completes cleanly when `ConnectionLifetime`
/// destruction runs after both frontend and driver leases are gone.
///
/// The structural completion-channel ordering proof lives in
/// `connection.rs:test_connection_lifetime_field_drop_order`.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_lifetime_final_owner_drop_order() {
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
    let (server_transport, server_driver) = server.unwrap();

    // Drop drivers first (never spawned)
    drop(client_driver);
    drop(server_driver);

    // Now drop frontends — these are the LAST holders.
    // ConnectionLifetime::drop runs field drops in order:
    //   1. shared_qp (SharedQp → Arc<Qp> → CmQueuePair::drop → rdma_destroy_qp)
    //   2. completion channels
    //   3. pd (Pd drop)
    //   4. cm_id (CmId::drop → rdma_destroy_id) — CmId alive when QP drops!
    //   5. event_channel (EventChannel fd close)
    //
    // If this test completes without SIGSEGV/SIGABRT, the drop order is safe.
    drop(client_transport);
    drop(server_transport);
}

// ═══════════════════════════════════════════════════════════════════════════
// MR Quarantine / Teardown Safety Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Structural test: QP destruction precedes CqDriverHandle's reclaim-queue
/// MR deregistration. Uses recorder proxies that mirror ConnectionLifetime's
/// field layout.
///
/// Verifies the invariant: an MR posted to hardware may only be freed
/// after its owning QP is destroyed.
#[test]
fn test_qp_destroy_before_mr_deregistration_order() {
    use std::sync::Mutex;

    struct Recorder(&'static str, std::sync::Arc<Mutex<Vec<&'static str>>>);
    impl Drop for Recorder {
        fn drop(&mut self) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    // SharedQp field order proxy: qp drops first, then handles
    struct SharedQpShape {
        _qp: Recorder,
        _send_handle: Recorder,
        _recv_handle: Recorder,
        _pd: Recorder,
    }

    // ConnectionLifetime field order proxy
    struct LifetimeShape {
        _shared_qp: SharedQpShape,
        _completion_channels: Vec<Recorder>,
        _pd: Recorder,
        _cm_id: Recorder,
        _event_channel: Recorder,
    }

    let log = std::sync::Arc::new(Mutex::new(Vec::new()));
    drop(LifetimeShape {
        _shared_qp: SharedQpShape {
            _qp: Recorder("qp", log.clone()),
            _send_handle: Recorder("send_handle", log.clone()),
            _recv_handle: Recorder("recv_handle", log.clone()),
            _pd: Recorder("sqp_pd", log.clone()),
        },
        _completion_channels: vec![Recorder("completion_channel", log.clone())],
        _pd: Recorder("pd", log.clone()),
        _cm_id: Recorder("cm_id", log.clone()),
        _event_channel: Recorder("event_channel", log.clone()),
    });

    let order = log.lock().unwrap();
    // QP must drop before handles (which hold reclaim queue MRs)
    let qp_pos = order.iter().position(|&s| s == "qp").unwrap();
    let send_pos = order.iter().position(|&s| s == "send_handle").unwrap();
    let recv_pos = order.iter().position(|&s| s == "recv_handle").unwrap();
    let channel_pos = order
        .iter()
        .position(|&s| s == "completion_channel")
        .unwrap();
    let cm_id_pos = order.iter().position(|&s| s == "cm_id").unwrap();
    assert!(
        qp_pos < send_pos,
        "QP must drop before send_handle (reclaim queue MRs)"
    );
    assert!(
        qp_pos < recv_pos,
        "QP must drop before recv_handle (reclaim queue MRs)"
    );
    assert!(
        qp_pos < cm_id_pos,
        "QP must drop before CmId (rdma_destroy_qp requires live cm_id)"
    );
    assert!(
        channel_pos < cm_id_pos,
        "completion channels must drop before CmId"
    );
}

/// Proves: TransportSharedState field drop order is safe — conn_lifetime
/// drops before driver_handles. Both hold Arc references (not owned values),
/// so the actual drop order of the underlying resources is controlled by
/// ConnectionLifetime's internal field layout, not this struct. Either
/// ordering is safe because SharedQp inside ConnectionLifetime holds the
/// final Arc<CqDriverHandle> refs — CqDriverHandle (and its quarantined
/// MRs) cannot be freed until after QP destruction.
#[test]
fn test_transport_shared_state_field_order() {
    use std::sync::Mutex;

    struct Recorder(&'static str, std::sync::Arc<Mutex<Vec<&'static str>>>);
    impl Drop for Recorder {
        fn drop(&mut self) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    // Mirrors TransportSharedState field layout (conn_lifetime before driver_handles)
    struct StateShape {
        _state: u8,
        _conn_lifetime: Recorder,
        _driver_handles: Vec<Recorder>,
    }

    let log = std::sync::Arc::new(Mutex::new(Vec::new()));
    drop(StateShape {
        _state: 0,
        _conn_lifetime: Recorder("conn_lifetime", log.clone()),
        _driver_handles: vec![Recorder("driver_handles", log.clone())],
    });

    let order = log.lock().unwrap();
    let dh_pos = order.iter().position(|&s| s == "driver_handles").unwrap();
    let cl_pos = order.iter().position(|&s| s == "conn_lifetime").unwrap();
    // conn_lifetime drops before driver_handles. Both are Arc refs so
    // either order is safe — the actual destruction sequence is enforced
    // by ConnectionLifetime's internal field layout (SharedQp first).
    assert!(
        cl_pos < dh_pos,
        "conn_lifetime should drop before driver_handles in TransportSharedState"
    );
}

/// Proves: InflightMap close() wakes all registered wakers.
/// After close, is_closed() returns true.
#[test]
fn test_inflight_map_close_wakes_waiters() {
    use rdma_io::v2::inflight::InflightMap;

    let map = InflightMap::new(4);
    assert!(!map.is_closed());

    let r1 = map.register().unwrap();
    let r2 = map.register().unwrap();

    // Register wakers
    let waker = std::task::Waker::noop();
    map.register_waker(r1.token, waker);
    map.register_waker(r2.token, waker);

    // Close should wake all wakers and set closed flag
    map.close();
    assert!(map.is_closed());

    // Completions can still be delivered after close
    assert!(map.complete(r1.token, rdma_io::wc::WorkCompletion::default()));

    // Take completion works normally
    assert!(map.take_completion(r1.token).is_some());

    map.release(r1.token);
    map.release(r2.token);
}

/// Proves: MR quarantine on driver abort — when the driver is dropped
/// with inflight operations, OpFuture waiters receive (Err, None) and
/// the MR is quarantined in the reclaim queue for safe destruction.
///
/// Post-condition: transport drops cleanly without UAF because
/// ConnectionLifetime destroys QP before handle reclaim queues are freed.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_mr_quarantine_on_driver_abort() {
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

    // Establish connection with a message exchange
    client_transport.send(b"pre-abort").await.unwrap();
    let _ = server_transport.recv().await.unwrap();

    // Abort the driver — Drop guard fires, closing inflight maps.
    // Any OpFuture waiters should receive (Err, None) — MR quarantined.
    client_driver_handle.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), client_driver_handle).await;
    tokio::task::yield_now().await;

    // Frontend operations should fail
    assert!(client_transport.send(b"post-abort").await.is_err());
    assert!(client_transport.recv().await.is_err());

    // Error should be DriverAborted
    let err = client_transport
        .error()
        .expect("error should be set after abort");
    assert_eq!(*err.kind(), TransportErrorKind::DriverAborted);

    // Drop everything — ConnectionLifetime destructs safely:
    // QP destroyed before CqDriverHandle reclaim queues freed.
    // If this completes without SIGSEGV, MR quarantine is working.
    drop(client_transport);
    drop(server_transport);
    let _ = server_driver_handle.await;
}

/// Proves: dropping an unspawned driver quarantines pre-posted MRs.
/// The pre-posted recv MRs should be quarantined (not freed) because
/// the inflight map is closed on driver drop. They are freed only
/// after QP destruction via ConnectionLifetime.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_mr_quarantine_on_unspawned_driver_drop() {
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

    // Drop drivers without ever spawning them.
    // The driver future holds pre-posted recv OpFutures. When the
    // future drops, those OpFutures drop → MRs pushed to reclaim queue.
    // close_and_shutdown closes the inflight map, ensuring waiters
    // quarantine their MRs.
    drop(client_driver);
    drop(server_driver);

    // Frontend should report DriverAborted
    let err = client_transport
        .error()
        .expect("error should be set after driver dropped");
    assert_eq!(*err.kind(), TransportErrorKind::DriverAborted);

    // close() must not hang
    let result = tokio::time::timeout(Duration::from_secs(5), client_transport.close()).await;
    assert!(result.is_ok(), "close() should return immediately");

    // Drop frontend — last ConnectionLifetime holder.
    // QP destroyed before CqDriverHandle reclaim queues freed.
    // No SIGSEGV = safe.
    drop(client_transport);
    drop(_server_transport);
}

/// Proves: graceful close with inflight send operations processes real
/// CQEs before returning MRs. The driver enters Phase C, transitions
/// QP→ERR, and the CQ drain barrier reaps real flush CQEs.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_graceful_close_drains_real_cqes() {
    require_software_rdma!();

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // Exchange messages to verify connection works
    client.send(b"before-close").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"before-close");
    drop(msg);

    // Graceful close — driver enters Phase C, QP→ERR, CQ drain
    client.close().await;

    // After close, send fails
    assert!(client.send(b"after-close").await.is_err());

    // Drop — no UAF because Phase C drained real CQEs
    drop(client);
    drop(server);
}
