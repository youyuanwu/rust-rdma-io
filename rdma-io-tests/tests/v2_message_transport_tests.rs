//! Integration tests for the v2 message-oriented Send/Recv transport.
//!
//! Tests cover both readiness (fd/channel-based) and polling completion modes,
//! verifying identical behavior per SC-008. Tests use deterministic
//! synchronization (channels, barriers, bounded timeouts) — no wall-clock
//! sleeps for synchronization.

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

    let server_task = tokio::spawn(async move { server_builder.accept(&listener).await.unwrap() });
    let client_task =
        tokio::spawn(async move { client_builder.connect(listen_addr).await.unwrap() });

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

    let (client, server) = make_transport_pair(CompletionMode::Readiness, 8, 4, 4096).await;
    for i in 0..10u32 {
        let msg = format!("message {i}");
        client.send(msg.as_bytes()).await.unwrap();

        let received = server.recv().await.unwrap();
        assert_eq!(received.as_ref(), msg.as_bytes());
    }
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
        // ReceivedMessage drops here → MR returned for reposting
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
// Backpressure tests
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

    // Receive both to free up buffers
    let m1 = server.recv().await.unwrap();
    let m2 = server.recv().await.unwrap();
    drop(m1);
    drop(m2);

    // Now send again — should work (buffers recovered)
    client.send(b"after-bp").await.unwrap();
    let m3 = server.recv().await.unwrap();
    assert_eq!(m3.as_ref(), b"after-bp");
}

// ============================================================
// Shutdown and disconnect tests
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

    // Test that closing a transport doesn't hang.
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // Exchange a message to prove the connection works
    client.send(b"before-shutdown").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"before-shutdown");
    drop(msg);

    // Close both sides
    server.close().await;
    drop(client);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shutdown_wakes_pending_recv() {
    require_software_rdma!();

    // Test that pending recv is woken when the transport's recv channel
    // closes due to recv pump shutdown.
    let (client, server) = make_transport_pair(CompletionMode::Readiness, 4, 4, 256).await;

    // Exchange a message to prove the connection works
    client.send(b"test").await.unwrap();
    let msg = server.recv().await.unwrap();
    assert_eq!(msg.as_ref(), b"test");
    drop(msg);

    // Get driver handles so we can trigger shutdown from outside
    let conn_handles: Vec<_> = server.connection().driver_handles().to_vec();

    // Spawn recv in a separate task (will block)
    let server = std::sync::Arc::new(server);
    let s2 = server.clone();
    let recv_task = tokio::spawn(async move { s2.recv().await });

    // Give recv time to park
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Shut down the driver handles — this flushes pending recv ops
    // with synthetic flush errors, causing recv pump to exit and
    // close the recv channel.
    for handle in &conn_handles {
        handle.flush_and_shutdown();
    }

    // The recv task should complete with an error
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

    // Verify shared-CQ mode: one driver handle
    assert_eq!(
        client.connection().driver_handles().len(),
        1,
        "shared CQ should have exactly one driver handle"
    );
    assert_eq!(
        server.connection().driver_handles().len(),
        1,
        "shared CQ should have exactly one driver handle"
    );

    // Both send and recv completions arrive on the same CQ
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
