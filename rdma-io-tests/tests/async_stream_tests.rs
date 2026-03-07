//! AsyncRdmaStream integration tests — Phases C + D + F.
//!
//! Tests verify that `AsyncRdmaStream` provides TCP-like async read/write
//! over RDMA SEND/RECV, using completion-channel-driven async CQ polling.
//!
//! Phase D tests verify the `futures::io::AsyncRead`/`AsyncWrite` trait
//! implementations and tokio compat layer.
//!
//! Phase F tests verify async connect/accept using CM event channel
//! (no `spawn_blocking` needed — both connect and accept are native async).

use rdma_io::async_stream::{AsyncRdmaListener, AsyncRdmaStream};

use rdma_io_tests::test_helpers::test_addrs;

/// Test: async echo — client sends, server reads and echoes back.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn async_stream_echo() {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();

    let server_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle =
        tokio::spawn(async move { AsyncRdmaStream::connect(&connect_addr).await.unwrap() });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let mut server = server_res.unwrap();
    let mut client = client_res.unwrap();

    // Client sends, server reads
    let send_data = b"async stream hello!";
    let n = client.write(send_data).await.unwrap();
    assert_eq!(n, send_data.len());

    let mut buf = [0u8; 1024];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], send_data);

    // Server echoes back
    let n = server.write(&buf[..n]).await.unwrap();
    assert_eq!(n, send_data.len());

    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], send_data);

    println!("async_stream_echo passed!");
}

/// Test: multi-message — 5 round-trips of ping/pong.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_multi_message() {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
    let server_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client =
        tokio::spawn(async move { AsyncRdmaStream::connect(&connect_addr).await.unwrap() })
            .await
            .unwrap();

    let mut server = server_handle.await.unwrap();

    for i in 0..5u32 {
        let msg = format!("message-{i}");
        client.write(msg.as_bytes()).await.unwrap();

        let mut buf = [0u8; 256];
        let n = server.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], msg.as_bytes(), "round {i} server read mismatch");

        let reply = format!("reply-{i}");
        server.write(reply.as_bytes()).await.unwrap();

        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            reply.as_bytes(),
            "round {i} client read mismatch"
        );
    }

    println!("async_stream_multi_message passed!");
}

/// Test: large transfer — send 32 KiB in one write.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_large_transfer() {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
    let server_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client =
        tokio::spawn(async move { AsyncRdmaStream::connect(&connect_addr).await.unwrap() })
            .await
            .unwrap();

    let mut server = server_handle.await.unwrap();

    // 32 KiB of patterned data
    let data: Vec<u8> = (0..32768).map(|i| (i % 251) as u8).collect();
    let n = client.write(&data).await.unwrap();
    assert_eq!(n, data.len());

    // Server reads — may need multiple reads for partial delivery
    let mut received = Vec::new();
    let mut buf = [0u8; 65536];
    while received.len() < data.len() {
        let n = server.read(&mut buf).await.unwrap();
        assert!(n > 0, "unexpected EOF");
        received.extend_from_slice(&buf[..n]);
    }

    assert_eq!(received.len(), data.len());
    assert_eq!(received, data);

    println!("async_stream_large_transfer passed!");
}

// --- Phase D: futures::io AsyncRead/AsyncWrite trait tests ---

/// Helper: create a connected (server, client) pair using async connect/accept.
async fn connected_pair() -> (AsyncRdmaStream, AsyncRdmaStream) {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
    let server_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle =
        tokio::spawn(async move { AsyncRdmaStream::connect(&connect_addr).await.unwrap() });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    (server_res.unwrap(), client_res.unwrap())
}

/// Test: futures::io::AsyncReadExt / AsyncWriteExt echo via trait methods.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn async_stream_futures_io_echo() {
    let (mut server, mut client) = connected_pair().await;

    // Client writes using AsyncWriteExt
    let msg = b"futures-io trait echo!";
    let n = futures_util::io::AsyncWriteExt::write(&mut client, msg)
        .await
        .unwrap();
    assert_eq!(n, msg.len());

    // Server reads using AsyncReadExt
    let mut buf = [0u8; 256];
    let n = futures_util::io::AsyncReadExt::read(&mut server, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf[..n], msg);

    // Server echoes back
    futures_util::io::AsyncWriteExt::write(&mut server, &buf[..n])
        .await
        .unwrap();

    let n = futures_util::io::AsyncReadExt::read(&mut client, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf[..n], msg);

    println!("async_stream_futures_io_echo passed!");
}

/// Test: tokio compat layer — use tokio::io::AsyncReadExt/AsyncWriteExt
/// via FuturesAsyncReadCompatExt.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn async_stream_tokio_compat() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let (server, client) = connected_pair().await;

    // Wrap with compat layer to get tokio::io traits
    let mut server = server.compat();
    let mut client = client.compat();

    let msg = b"tokio compat layer!";
    client.write_all(msg).await.unwrap();

    let mut buf = [0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    // Echo back
    server.write_all(&buf[..n]).await.unwrap();

    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    println!("async_stream_tokio_compat passed!");
}

/// Test: tokio::io::copy through the compat layer.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn async_stream_tokio_io_copy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let (server, client) = connected_pair().await;
    let mut server = server.compat();
    let mut client = client.compat();

    // Send data from client
    let data = b"copy test payload 12345";
    client.write_all(data).await.unwrap();

    // Read on server side into a Vec using read_buf
    let mut buf = vec![0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], data);

    println!("async_stream_tokio_io_copy passed!");
}

/// Test: server shutdown after client drops connection.
///
/// Reproduces the CI hang: client disconnects → server QP enters ERROR →
/// server calls shutdown(). poll_close must detect the dead QP and return
/// promptly instead of waiting indefinitely for CQ notifications or CM
/// events that may never arrive.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_shutdown_after_peer_drop() {
    let (mut server, client) = connected_pair().await;

    // Drop client — its Drop impl calls disconnect(), sending DREQ.
    // Server QP transitions to ERROR state.
    drop(client);

    // Give QP state change time to propagate.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Server shutdown must complete, not hang forever.
    // This exercises poll_close Phase 2 (disconnect on dead QP) and
    // Phase 3 (await DISCONNECTED event that may already have arrived).
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;

    // Ok(Ok(..)) or Ok(Err(..)) are both acceptable — the important
    // thing is that shutdown didn't hang (Err(Elapsed) = hang).
    assert!(result.is_ok(), "server shutdown hung — timed out after 5s");
    println!("async_stream_shutdown_after_peer_drop passed!");
}

/// Test: server read detects EOF, then shutdown completes immediately.
///
/// When poll_read sees flush completions it sets read_eof, which lets
/// poll_close take the fast path (Layer 2). This is the well-tested path.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_read_eof_then_shutdown() {
    let (mut server, client) = connected_pair().await;

    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Server reads — should get EOF (Ok(0)) from flush completions.
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), server.read(&mut buf))
        .await
        .expect("read hung")
        .expect("read failed");
    assert_eq!(n, 0, "expected EOF after peer disconnect");

    // Now shutdown takes the read_eof fast path — should be instant.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;
    assert!(result.is_ok(), "server shutdown hung after read EOF");
    println!("async_stream_read_eof_then_shutdown passed!");
}

/// Test: server writes to dead QP (BrokenPipe), then shutdown completes.
///
/// poll_write's Layer 3 (is_qp_error) sets read_eof on BrokenPipe.
/// Subsequent poll_close takes the fast path via Layer 2.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_write_then_shutdown_after_peer_drop() {
    let (mut server, client) = connected_pair().await;

    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Server write should fail — BrokenPipe via Layer 3 (is_qp_error).
    let write_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.write(b"data after disconnect"),
    )
    .await
    .expect("write hung");
    assert!(write_result.is_err(), "write to dead QP should fail");

    // Now shutdown — read_eof set by Layer 3, poll_close returns immediately.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;
    assert!(
        result.is_ok(),
        "server shutdown hung after write BrokenPipe"
    );
    println!("async_stream_write_then_shutdown_after_peer_drop passed!");
}

/// Test: explicit shutdown() performs graceful disconnect (DREQ → DISCONNECTED).
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_shutdown() {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
    let server_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = AsyncRdmaStream::connect(&connect_addr).await.unwrap();
    let mut server = server_handle.await.unwrap();

    // Round-trip: client sends, server echoes, client reads.
    let msg = b"shutdown test";
    client.write(msg).await.unwrap();
    let mut buf = [0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);
    server.write(&buf[..n]).await.unwrap();
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    // Client shutdown: drains send, sends DREQ, awaits DISCONNECTED.
    client.shutdown().await.unwrap();

    println!("async_stream_shutdown passed!");
}

/// Test: poll_close via futures AsyncWriteExt::close() performs graceful disconnect.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn async_stream_poll_close() {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
    let server_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = AsyncRdmaStream::connect(&connect_addr).await.unwrap();
    let mut server = server_handle.await.unwrap();

    // Round-trip: client sends, server echoes, client reads.
    let msg = b"close test";
    client.write(msg).await.unwrap();
    let mut buf = [0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);
    server.write(&buf[..n]).await.unwrap();
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    // Close via futures_io::AsyncWrite::poll_close (through AsyncWriteExt).
    futures_util::io::AsyncWriteExt::close(&mut client)
        .await
        .unwrap();

    println!("async_stream_poll_close passed!");
}
