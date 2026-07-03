//! AsyncRdmaStream integration tests.
//!
//! Each test has a generic body (`test_name<B: TransportBuilder>`) and two
//! concrete entry points: `test_name_default` (Send/Recv) and `test_name_ring`
//! (Ring buffer). The ring variant is skipped on iWARP.

use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::TransportBuilder;

use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::connect_addr_for;

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

/// Create a connected (server, client) stream pair using any transport builder.
/// Retries bind + client connect on EADDRINUSE (siw async port release).
async fn connected_pair<B: TransportBuilder>(
    builder: B,
) -> (AsyncRdmaStream<B::Transport>, AsyncRdmaStream<B::Transport>) {
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());
    let builder2 = builder.clone();

    let server_handle =
        tokio::spawn(
            async move { AsyncRdmaStream::new(builder2.accept(&listener).await.unwrap()) },
        );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let client_handle = tokio::spawn(async move {
        let transport =
            rdma_io_tests::test_helpers::connect_with_retry(&builder, &connect_addr).await;
        AsyncRdmaStream::new(transport)
    });

    let (s, c) = tokio::join!(server_handle, client_handle);
    (s.unwrap(), c.unwrap())
}

// ===========================================================================
// echo — client sends a message, server reads and echoes it back.
// ===========================================================================

async fn echo<B: TransportBuilder>(builder: B) {
    let (mut server, mut client) = connected_pair(builder).await;

    let send_data = b"async stream hello!";
    let n = client.write(send_data).await.unwrap();
    assert_eq!(n, send_data.len());

    let mut buf = [0u8; 1024];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], send_data);

    let n = server.write(&buf[..n]).await.unwrap();
    assert_eq!(n, send_data.len());

    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], send_data);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn echo_default() {
    echo(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn echo_ring() {
    require_no_iwarp!();
    echo(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn echo_read_ring() {
    require_no_iwarp!();
    echo(ReadRingConfig::default()).await;
}

// ===========================================================================
// multi_message — 5 round-trips of ping/pong.
// ===========================================================================

async fn multi_message<B: TransportBuilder>(builder: B) {
    let (mut server, mut client) = connected_pair(builder).await;

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
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn multi_message_default() {
    multi_message(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn multi_message_ring() {
    require_no_iwarp!();
    multi_message(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn multi_message_read_ring() {
    require_no_iwarp!();
    multi_message(ReadRingConfig::default()).await;
}

// ===========================================================================
// large_transfer — send 32 KiB, verify integrity.
// Uses write_all + concurrent reader for transport-independence
// (ring transport caps each send to max_message_size).
// ===========================================================================

async fn large_transfer<B: TransportBuilder>(builder: B) {
    let (server, client) = connected_pair(builder).await;

    let total = 32768usize;
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    let send_data = data.clone();

    let writer = tokio::spawn(async move {
        let mut client = client;
        client.write_all(&send_data).await.unwrap();
        client
    });

    let reader = tokio::spawn(async move {
        let mut server = server;
        let mut received = Vec::with_capacity(total);
        let mut buf = [0u8; 65536];
        while received.len() < total {
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), server.read(&mut buf))
                .await
                .expect("read timed out")
                .expect("read failed");
            assert!(n > 0, "unexpected EOF at {} bytes", received.len());
            received.extend_from_slice(&buf[..n]);
        }
        (server, received)
    });

    let (writer_res, reader_res) = tokio::join!(writer, reader);
    let _client = writer_res.unwrap();
    let (_server, received) = reader_res.unwrap();

    assert_eq!(received.len(), total);
    assert_eq!(received, data);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn large_transfer_default() {
    large_transfer(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn large_transfer_ring() {
    require_no_iwarp!();
    large_transfer(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn large_transfer_read_ring() {
    require_no_iwarp!();
    large_transfer(ReadRingConfig::default()).await;
}

// ===========================================================================
// pipelined_transfer — many small writes over a depth-sized stream, exercising
// the N-deep send pipeline: writes post-and-return (multiple sends outstanding),
// backpressure once the pipeline is full, ordering preserved via recv_stash,
// and poll_close drains all in-flight sends. Verifies stream integrity.
// ===========================================================================

async fn pipelined_transfer<B: TransportBuilder>(builder: B) {
    let (server, client) = connected_pair(builder).await;

    // Many small messages so several sends are in flight at once (deeper than
    // any single write). 512 * 1000 B = 500 KB total.
    let chunk = vec![0u8; 1000];
    let chunk_len = chunk.len();
    let chunks = 512usize;
    let total = chunk_len * chunks;

    let writer = tokio::spawn(async move {
        let mut client = client;
        for i in 0..chunks {
            // Vary the first byte per chunk so ordering errors are detectable.
            let mut c = chunk.clone();
            c[0] = (i % 251) as u8;
            client.write_all(&c).await.unwrap();
        }
        // Graceful close drains the in-flight send pipeline.
        client.shutdown().await.unwrap();
        client
    });

    let reader = tokio::spawn(async move {
        let mut server = server;
        let mut received = Vec::with_capacity(total);
        let mut buf = [0u8; 65536];
        while received.len() < total {
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), server.read(&mut buf))
                .await
                .expect("read timed out")
                .expect("read failed");
            assert!(n > 0, "unexpected EOF at {} bytes", received.len());
            received.extend_from_slice(&buf[..n]);
        }
        received
    });

    let (writer_res, reader_res) = tokio::join!(writer, reader);
    let _client = writer_res.unwrap();
    let received = reader_res.unwrap();

    assert_eq!(received.len(), total, "byte count mismatch");
    // Verify per-chunk ordering marker survived the pipeline.
    for i in 0..chunks {
        assert_eq!(
            received[i * chunk_len],
            (i % 251) as u8,
            "chunk {i} out of order or corrupted"
        );
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn pipelined_transfer_default() {
    pipelined_transfer(SendRecvConfig::stream_with_depth(16)).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn pipelined_transfer_ring() {
    require_no_iwarp!();
    pipelined_transfer(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn pipelined_transfer_read_ring() {
    require_no_iwarp!();
    pipelined_transfer(ReadRingConfig::default()).await;
}

// ===========================================================================
// concurrent_write_no_deadlock — both peers write many small messages before
// either reads (the HTTP/2 write-priority pattern that wedged read-ring at
// in_flight > 1). Fix A's write-blocked drain copies the received bytes out and
// reposts the recv slot immediately, so a write-blocked endpoint keeps freeing
// its peer's flow control; without it, read-ring deadlocks and this times out.
//
// Read-ring only: the pattern (both sides write `count` messages before either
// reads) is what the bug requires. `count` must exceed the read-ring doorbell
// pool (43) to force the blocked-write path, yet stay within the stream stash
// cap (64) so each peer can buffer the other's whole stream without reading.
// (credit-ring's credit window and send-recv's recv-buffer pool make this exact
// no-read pattern a different question; those transports don't have this bug and
// are already exercised by the pipelined/large-transfer tests.)
// See docs/bugs/read-ring-concurrent-stream-deadlock.md.
// ===========================================================================

async fn concurrent_write_no_deadlock<B: TransportBuilder>(builder: B) {
    let (server, client) = connected_pair(builder).await;

    // count > the read-ring doorbell pool (43) forces the blocked-write path;
    // count <= the stream stash cap (64) lets each peer buffer the other's whole
    // stream without reading, so both writers complete via fix A's eager release.
    let count = 50usize;
    let msg = 64usize;

    // Distinct per-message marker byte per direction so ordering is checkable.
    let cdata: Vec<u8> = (0..count).map(|i| (i % 251) as u8).collect();
    let sdata: Vec<u8> = (0..count).map(|i| ((i * 3 + 7) % 251) as u8).collect();
    let cw = cdata.clone();
    let sw = sdata.clone();

    // Both sides write their full payload before reading anything.
    let writer_c = tokio::spawn(async move {
        let mut client = client;
        for &b in &cw {
            client.write_all(&vec![b; msg]).await.unwrap();
        }
        client
    });
    let writer_s = tokio::spawn(async move {
        let mut server = server;
        for &b in &sw {
            server.write_all(&vec![b; msg]).await.unwrap();
        }
        server
    });

    // Without fix A this join never resolves (both peers wedged, no wakeup).
    let (client, server) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        (writer_c.await.unwrap(), writer_s.await.unwrap())
    })
    .await
    .expect("concurrent writes deadlocked (see read-ring-concurrent-stream-deadlock.md)");

    // Drain each direction and verify per-message markers survived in order.
    let reader_c = tokio::spawn(async move {
        let mut client = client;
        let mut got = Vec::with_capacity(count * msg);
        let mut buf = [0u8; 4096];
        while got.len() < count * msg {
            let n = client.read(&mut buf).await.unwrap();
            assert!(n > 0, "unexpected EOF draining client");
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    let reader_s = tokio::spawn(async move {
        let mut server = server;
        let mut got = Vec::with_capacity(count * msg);
        let mut buf = [0u8; 4096];
        while got.len() < count * msg {
            let n = server.read(&mut buf).await.unwrap();
            assert!(n > 0, "unexpected EOF draining server");
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    let (c_got, s_got) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        (reader_c.await.unwrap(), reader_s.await.unwrap())
    })
    .await
    .expect("reads timed out draining the buffered streams");

    // Client received what the server wrote (sdata), and vice versa.
    for i in 0..count {
        assert_eq!(
            c_got[i * msg],
            sdata[i],
            "client recv chunk {i} out of order"
        );
        assert_eq!(
            s_got[i * msg],
            cdata[i],
            "server recv chunk {i} out of order"
        );
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn concurrent_write_no_deadlock_read_ring() {
    require_no_iwarp!();
    concurrent_write_no_deadlock(ReadRingConfig::default()).await;
}

// ===========================================================================
// futures_io_echo — echo via futures::io::AsyncReadExt / AsyncWriteExt traits.
// ===========================================================================

async fn futures_io_echo<B: TransportBuilder>(builder: B) {
    let (mut server, mut client) = connected_pair(builder).await;

    let msg = b"futures-io trait echo!";
    let n = futures_util::io::AsyncWriteExt::write(&mut client, msg)
        .await
        .unwrap();
    assert_eq!(n, msg.len());

    let mut buf = [0u8; 256];
    let n = futures_util::io::AsyncReadExt::read(&mut server, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf[..n], msg);

    futures_util::io::AsyncWriteExt::write(&mut server, &buf[..n])
        .await
        .unwrap();

    let n = futures_util::io::AsyncReadExt::read(&mut client, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf[..n], msg);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn futures_io_echo_default() {
    futures_io_echo(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn futures_io_echo_ring() {
    require_no_iwarp!();
    futures_io_echo(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn futures_io_echo_read_ring() {
    require_no_iwarp!();
    futures_io_echo(ReadRingConfig::default()).await;
}

// ===========================================================================
// tokio_compat — tokio::io traits via FuturesAsyncReadCompatExt.
// ===========================================================================

async fn tokio_compat<B: TransportBuilder>(builder: B) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let (server, client) = connected_pair(builder).await;
    let mut server = server.compat();
    let mut client = client.compat();

    let msg = b"tokio compat layer!";
    client.write_all(msg).await.unwrap();

    let mut buf = [0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    server.write_all(&buf[..n]).await.unwrap();

    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_compat_default() {
    tokio_compat(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_compat_ring() {
    require_no_iwarp!();
    tokio_compat(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_compat_read_ring() {
    require_no_iwarp!();
    tokio_compat(ReadRingConfig::default()).await;
}

// ===========================================================================
// tokio_io_copy — tokio::io::copy through the compat layer.
// ===========================================================================

async fn tokio_io_copy<B: TransportBuilder>(builder: B) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let (server, client) = connected_pair(builder).await;
    let mut server = server.compat();
    let mut client = client.compat();

    let data = b"copy test payload 12345";
    client.write_all(data).await.unwrap();

    let mut buf = vec![0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], data);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_io_copy_default() {
    tokio_io_copy(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_io_copy_ring() {
    require_no_iwarp!();
    tokio_io_copy(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_io_copy_read_ring() {
    require_no_iwarp!();
    tokio_io_copy(ReadRingConfig::default()).await;
}

// ===========================================================================
// shutdown_after_peer_drop — server shutdown must not hang after client drops.
// ===========================================================================

async fn shutdown_after_peer_drop<B: TransportBuilder>(builder: B) {
    let (mut server, client) = connected_pair(builder).await;

    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;
    assert!(result.is_ok(), "server shutdown hung — timed out after 5s");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn shutdown_after_peer_drop_default() {
    shutdown_after_peer_drop(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn shutdown_after_peer_drop_ring() {
    require_no_iwarp!();
    shutdown_after_peer_drop(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn shutdown_after_peer_drop_read_ring() {
    require_no_iwarp!();
    shutdown_after_peer_drop(ReadRingConfig::default()).await;
}

// ===========================================================================
// read_eof_then_shutdown — server reads EOF after peer drop, then shutdown.
// ===========================================================================

async fn read_eof_then_shutdown<B: TransportBuilder>(builder: B) {
    let (mut server, client) = connected_pair(builder).await;

    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), server.read(&mut buf))
        .await
        .expect("read hung")
        .expect("read failed");
    assert_eq!(n, 0, "expected EOF after peer disconnect");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;
    assert!(result.is_ok(), "server shutdown hung after read EOF");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_eof_then_shutdown_default() {
    read_eof_then_shutdown(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_eof_then_shutdown_ring() {
    require_no_iwarp!();
    read_eof_then_shutdown(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_eof_then_shutdown_read_ring() {
    require_no_iwarp!();
    read_eof_then_shutdown(ReadRingConfig::default()).await;
}

// ===========================================================================
// write_then_shutdown_after_peer_drop — write to dead QP fails, then shutdown.
// ===========================================================================

async fn write_then_shutdown_after_peer_drop<B: TransportBuilder>(builder: B) {
    let (mut server, client) = connected_pair(builder).await;

    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Write until the dead QP is detected. The first write may succeed
    // (ring transport can post a WR before the disconnect propagates),
    // but subsequent writes must eventually fail.
    let mut write_failed = false;
    for _ in 0..10 {
        let write_result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.write(b"data after disconnect"),
        )
        .await
        .expect("write hung");
        if write_result.is_err() {
            write_failed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(write_failed, "write to dead QP should eventually fail");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;
    assert!(
        result.is_ok(),
        "server shutdown hung after write BrokenPipe"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn write_then_shutdown_after_peer_drop_default() {
    write_then_shutdown_after_peer_drop(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn write_then_shutdown_after_peer_drop_ring() {
    require_no_iwarp!();
    write_then_shutdown_after_peer_drop(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn write_then_shutdown_after_peer_drop_read_ring() {
    require_no_iwarp!();
    write_then_shutdown_after_peer_drop(ReadRingConfig::default()).await;
}

// ===========================================================================
// graceful_shutdown — round-trip then explicit shutdown (DREQ → DISCONNECTED).
// ===========================================================================

async fn graceful_shutdown<B: TransportBuilder>(builder: B) {
    let (mut server, mut client) = connected_pair(builder).await;

    let msg = b"shutdown test";
    client.write(msg).await.unwrap();
    let mut buf = [0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);
    server.write(&buf[..n]).await.unwrap();
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    client.shutdown().await.unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn graceful_shutdown_default() {
    graceful_shutdown(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn graceful_shutdown_ring() {
    require_no_iwarp!();
    graceful_shutdown(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn graceful_shutdown_read_ring() {
    require_no_iwarp!();
    graceful_shutdown(ReadRingConfig::default()).await;
}

// ===========================================================================
// poll_close — graceful disconnect via futures AsyncWriteExt::close().
// ===========================================================================

async fn poll_close<B: TransportBuilder>(builder: B) {
    let (mut server, mut client) = connected_pair(builder).await;

    let msg = b"close test";
    client.write(msg).await.unwrap();
    let mut buf = [0u8; 256];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);
    server.write(&buf[..n]).await.unwrap();
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], msg);

    futures_util::io::AsyncWriteExt::close(&mut client)
        .await
        .unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn poll_close_default() {
    poll_close(SendRecvConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn poll_close_ring() {
    require_no_iwarp!();
    poll_close(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn poll_close_read_ring() {
    require_no_iwarp!();
    poll_close(ReadRingConfig::default()).await;
}
