//! Integration tests for the v2 message-oriented Send/Recv transport.

use rdma_io::v2::*;
use rdma_io_tests::test_helpers::*;

macro_rules! require_software_rdma {
    () => {
        if !rdma_io_tests::test_helpers::has_software_rdma() {
            tracing::warn!("SKIPPED: no software RDMA device (rxe/siw)");
            return;
        }
    };
}

/// Helper to run a message transport test with both completion modes.
async fn with_transport_pair<F, Fut>(
    mode: CompletionMode,
    recv_bufs: usize,
    send_bufs: usize,
    buf_size: usize,
    test_fn: F,
) where
    F: FnOnce(MessageTransport, MessageTransport) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = bind_listener_with_retry().await;
    let listen_addr = listener.local_addr().unwrap();

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
        server_builder.accept(&listener).await.unwrap()
    });

    let client_task = tokio::spawn(async move {
        client_builder.connect(listen_addr).await.unwrap()
    });

    let (server, client) = tokio::join!(server_task, client_task);
    let server = server.unwrap();
    let client = client.unwrap();

    test_fn(client, server).await;
}

// ============================================================
// Readiness mode tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_single_message_readiness() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Readiness,
        8, 4, 4096,
        |client, server| async move {
            let msg = b"hello from client";
            client.send(msg).await.unwrap();

            let received = server.recv().await.unwrap();
            assert_eq!(received.as_ref(), msg);
            assert_eq!(received.len(), msg.len());

            // Send in reverse direction
            let reply = b"hello from server";
            server.send(reply).await.unwrap();
            let received = client.recv().await.unwrap();
            assert_eq!(received.as_ref(), reply);
        },
    )
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_multiple_messages_readiness() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Readiness,
        8, 4, 4096,
        |client, server| async move {
            for i in 0..10u32 {
                let msg = format!("message {i}");
                client.send(msg.as_bytes()).await.unwrap();

                let received = server.recv().await.unwrap();
                assert_eq!(received.as_ref(), msg.as_bytes());
            }
        },
    )
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_oversize_rejected() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Readiness,
        4, 4, 64,
        |client, _server| async move {
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
        },
    )
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_max_size_message() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Readiness,
        4, 4, 256,
        |client, server| async move {
            let exact = vec![42u8; 256];
            client.send(&exact).await.unwrap();

            let received = server.recv().await.unwrap();
            assert_eq!(received.as_ref(), &exact[..]);
            assert_eq!(received.len(), 256);
        },
    )
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_zero_length_message() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Readiness,
        4, 4, 256,
        |client, server| async move {
            client.send(b"").await.unwrap();

            let received = server.recv().await.unwrap();
            assert!(received.is_empty());
            assert_eq!(received.len(), 0);
        },
    )
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_buffer_reuse_beyond_pool() {
    require_software_rdma!();

    // 4 recv buffers, send 12 messages (3× pool depth)
    with_transport_pair(
        CompletionMode::Readiness,
        4, 4, 256,
        |client, server| async move {
            for i in 0..12u32 {
                let msg = format!("msg-{i}");
                client.send(msg.as_bytes()).await.unwrap();

                let received = server.recv().await.unwrap();
                assert_eq!(received.as_ref(), msg.as_bytes());
                // ReceivedMessage drops here, returning MR for reposting
            }
        },
    )
    .await;
}

// ============================================================
// Polling mode tests (SC-008: both modes identical)
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_single_message_polling() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Polling,
        8, 4, 4096,
        |client, server| async move {
            let msg = b"hello polling";
            client.send(msg).await.unwrap();

            let received = server.recv().await.unwrap();
            assert_eq!(received.as_ref(), msg);
        },
    )
    .await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_multiple_messages_polling() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Polling,
        8, 4, 4096,
        |client, server| async move {
            for i in 0..10u32 {
                let msg = format!("polling-{i}");
                client.send(msg.as_bytes()).await.unwrap();

                let received = server.recv().await.unwrap();
                assert_eq!(received.as_ref(), msg.as_bytes());
            }
        },
    )
    .await;
}

// ============================================================
// Cancellation tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_recv_cancel_no_message_loss() {
    require_software_rdma!();

    with_transport_pair(
        CompletionMode::Readiness,
        4, 4, 256,
        |client, server| async move {
            // Cancel a recv() that hasn't received anything
            let recv_result = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                server.recv(),
            )
            .await;
            assert!(recv_result.is_err()); // timed out

            // Now send a message
            client.send(b"after cancel").await.unwrap();

            // The next recv() should get it
            let msg = server.recv().await.unwrap();
            assert_eq!(msg.as_ref(), b"after cancel");
        },
    )
    .await;
}

// ============================================================
// Shutdown tests
// ============================================================

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_message_transport_drop_no_hang() {
    require_software_rdma!();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        with_transport_pair(
            CompletionMode::Readiness,
            4, 4, 256,
            |client, server| async move {
                // Exchange one message then drop both
                client.send(b"test").await.unwrap();
                let _ = server.recv().await.unwrap();
                // Both transports drop here
            },
        ),
    )
    .await;
    assert!(result.is_ok(), "transport drop should not hang");
}
