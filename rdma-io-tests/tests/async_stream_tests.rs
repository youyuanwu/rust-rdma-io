//! AsyncRdmaStream integration tests.
//!
//! Each test has a generic body (`test_name<B: TransportBuilder>`) and two
//! concrete entry points: `test_name_default` (Send/Recv) and `test_name_ring`
//! (Ring buffer). The ring variant is skipped on iWARP.

use rdma_io::async_stream::AsyncRdmaStream;
use rdma_io::rdma_ring_transport::RingConfig;
use rdma_io::rdma_transport::TransportConfig;
use rdma_io::transport::TransportBuilder;

use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::{bind_addr, connect_addr_for};

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

/// Create a connected (server, client) stream pair using any transport builder.
/// Retries client connect on EADDRINUSE (siw async port release).
async fn connected_pair<B: TransportBuilder>(
    builder: B,
) -> (AsyncRdmaStream<B::Transport>, AsyncRdmaStream<B::Transport>) {
    use rdma_io::async_cm::AsyncCmListener;

    let listener = AsyncCmListener::bind(&bind_addr()).unwrap();
    let connect_addr = connect_addr_for(listener.local_addr());
    let builder2 = builder.clone();

    let server_handle =
        tokio::spawn(
            async move { AsyncRdmaStream::new(builder2.accept(&listener).await.unwrap()) },
        );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let client_handle = tokio::spawn(async move {
        for attempt in 0u64..5 {
            match builder.connect(&connect_addr).await {
                Ok(transport) => return AsyncRdmaStream::new(transport),
                Err(e) => {
                    let is_addr_in_use = matches!(&e, rdma_io::Error::Verbs(io)
                        if io.raw_os_error() == Some(98));
                    if is_addr_in_use && attempt < 4 {
                        tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1)))
                            .await;
                        continue;
                    }
                    panic!("connect failed: {e}");
                }
            }
        }
        unreachable!()
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
    echo(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn echo_ring() {
    require_no_iwarp!();
    echo(RingConfig::default()).await;
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
    multi_message(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn multi_message_ring() {
    require_no_iwarp!();
    multi_message(RingConfig::default()).await;
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
    large_transfer(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn large_transfer_ring() {
    require_no_iwarp!();
    large_transfer(RingConfig::default()).await;
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
    futures_io_echo(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn futures_io_echo_ring() {
    require_no_iwarp!();
    futures_io_echo(RingConfig::default()).await;
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
    tokio_compat(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_compat_ring() {
    require_no_iwarp!();
    tokio_compat(RingConfig::default()).await;
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
    tokio_io_copy(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn tokio_io_copy_ring() {
    require_no_iwarp!();
    tokio_io_copy(RingConfig::default()).await;
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
    shutdown_after_peer_drop(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn shutdown_after_peer_drop_ring() {
    require_no_iwarp!();
    shutdown_after_peer_drop(RingConfig::default()).await;
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
    read_eof_then_shutdown(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_eof_then_shutdown_ring() {
    require_no_iwarp!();
    read_eof_then_shutdown(RingConfig::default()).await;
}

// ===========================================================================
// write_then_shutdown_after_peer_drop — write to dead QP fails, then shutdown.
// ===========================================================================

async fn write_then_shutdown_after_peer_drop<B: TransportBuilder>(builder: B) {
    let (mut server, client) = connected_pair(builder).await;

    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let write_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.write(b"data after disconnect"),
    )
    .await
    .expect("write hung");
    assert!(write_result.is_err(), "write to dead QP should fail");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server.shutdown()).await;
    assert!(
        result.is_ok(),
        "server shutdown hung after write BrokenPipe"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn write_then_shutdown_after_peer_drop_default() {
    write_then_shutdown_after_peer_drop(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn write_then_shutdown_after_peer_drop_ring() {
    require_no_iwarp!();
    write_then_shutdown_after_peer_drop(RingConfig::default()).await;
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
    graceful_shutdown(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn graceful_shutdown_ring() {
    require_no_iwarp!();
    graceful_shutdown(RingConfig::default()).await;
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
    poll_close(TransportConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn poll_close_ring() {
    require_no_iwarp!();
    poll_close(RingConfig::default()).await;
}
