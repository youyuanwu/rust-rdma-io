//! Transport integration tests.
//!
//! Generic test bodies that run on all three transports: `SendRecvTransport`,
//! `CreditRingTransport`, and `ReadRingTransport`. Each test has `_default`
//! (SendRecv), `_ring` (CreditRing), and `_read_ring` (ReadRing) entry points.

use std::future::poll_fn;
use std::task::Poll;

use rdma_io::async_cm::AsyncCmListener;
use rdma_io::credit_ring_transport::CreditRingConfig;
use rdma_io::read_ring_transport::ReadRingConfig;
use rdma_io::send_recv_transport::SendRecvConfig;
use rdma_io::transport::{RecvCompletion, Transport, TransportBuilder};
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::{bind_addr, connect_addr_for};

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

/// Create a connected (server, client) transport pair using any builder.
async fn ring_connected_pair<B: TransportBuilder>(config: B) -> (B::Transport, B::Transport) {
    // siw port release is async — bind can transiently fail with EADDRINUSE
    // right after disconnect-heavy tests, even on port 0. Retry with backoff.
    let listener = {
        let mut last_err = None;
        let mut listener_opt = None;
        for attempt in 0..5u32 {
            match AsyncCmListener::bind(&bind_addr()) {
                Ok(l) => {
                    listener_opt = Some(l);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        100 * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
        listener_opt.unwrap_or_else(|| panic!("bind failed after 5 attempts: {:?}", last_err))
    };
    let connect_addr = connect_addr_for(listener.local_addr());
    let config2 = config.clone();

    let server = tokio::spawn(async move { config2.accept(&listener).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let client = tokio::spawn(async move { config.connect(&connect_addr).await.unwrap() });
    let (s, c) = tokio::join!(server, client);
    (s.unwrap(), c.unwrap())
}

/// Send data and wait for send completion.
async fn send_and_complete<T: Transport>(transport: &mut T, data: &[u8]) -> usize {
    let n = transport.send_copy(data).unwrap();
    assert!(n > 0, "send_copy returned 0 — no space in ring");
    poll_fn(|cx| transport.poll_send_completion(cx))
        .await
        .unwrap();
    n
}

/// Drain all pending send completions (non-blocking).
async fn drain_send_completions<T: Transport>(transport: &mut T) {
    while let Some(Ok(())) = poll_fn(|cx| match transport.poll_send_completion(cx) {
        Poll::Ready(r) => Poll::Ready(Some(r)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
    {}
}

/// Receive one data completion, looping past credit-only / padding-only batches.
async fn recv_one<T: Transport>(transport: &mut T) -> RecvCompletion {
    let mut completions = [RecvCompletion::default(); 1];
    for _ in 0..100 {
        let n = poll_fn(|cx| transport.poll_recv(cx, &mut completions))
            .await
            .unwrap();
        if n > 0 {
            return completions[0];
        }
    }
    panic!("recv_one: no data after 100 poll_recv attempts");
}

/// Send data, retrying until the remote side frees space.
async fn send_after_repost<T: Transport>(transport: &mut T, data: &[u8], ctx: &str) -> usize {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // Non-blocking poll_recv to pick up credit messages (CreditRing) or
        // padding completions. No-op for ReadRing if no data pending.
        let mut completions = [RecvCompletion::default(); 8];
        let _ = poll_fn(|cx| match transport.poll_recv(cx, &mut completions) {
            Poll::Ready(r) => Poll::Ready(Some(r)),
            Poll::Pending => Poll::Ready(None),
        })
        .await;

        drain_send_completions(transport).await;

        let n = transport.send_copy(data).unwrap();
        if n > 0 {
            return n;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{ctx} — timed out waiting for space (5s)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// ===========================================================================
// connect_accept — verify local_addr/peer_addr after connect + accept.
// ===========================================================================

async fn connect_accept<B: TransportBuilder>(config: B) {
    let (server, client) = ring_connected_pair(config).await;
    assert!(server.local_addr().is_some());
    assert!(server.peer_addr().is_some());
    assert!(client.local_addr().is_some());
    assert!(client.peer_addr().is_some());
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn connect_accept_default() {
    connect_accept(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn connect_accept_ring() {
    require_no_iwarp!();
    connect_accept(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn connect_accept_read_ring() {
    require_no_iwarp!();
    connect_accept(ReadRingConfig::default()).await;
}

// ===========================================================================
// send_recv_single — send 1500 bytes, verify data integrity.
// ===========================================================================

async fn send_recv_single<B: TransportBuilder>(config: B) {
    let (mut server, mut client) = ring_connected_pair(config).await;

    let data: Vec<u8> = (0..1500).map(|i| (i % 251) as u8).collect();
    send_and_complete(&mut client, &data).await;

    let rc = recv_one(&mut server).await;
    assert_eq!(rc.byte_len, 1500);
    assert_eq!(&server.recv_buf(rc.buf_idx)[..rc.byte_len], &data[..]);
    server.repost_recv(rc.buf_idx).unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn send_recv_single_default() {
    send_recv_single(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn send_recv_single_ring() {
    require_no_iwarp!();
    send_recv_single(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn send_recv_single_read_ring() {
    require_no_iwarp!();
    send_recv_single(ReadRingConfig::default()).await;
}

// ===========================================================================
// send_recv_multi — send until backpressure, verify count ≤ max_outstanding.
// ===========================================================================

async fn send_recv_multi<B: TransportBuilder>(config: B, max_outstanding: usize) {
    let (mut _server, mut client) = ring_connected_pair(config).await;

    let data = vec![0xABu8; 1500];
    let mut sent_count = 0;
    for _ in 0..(max_outstanding + 5) {
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_) => sent_count += 1,
            Err(e) => panic!("unexpected send_copy error: {e}"),
        }
    }

    assert!(sent_count <= max_outstanding);
    assert!(sent_count > 0);
    assert_eq!(
        client.send_copy(&data).unwrap(),
        0,
        "should be backpressured"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn send_recv_multi_default() {
    let config = SendRecvConfig::datagram();
    send_recv_multi(config, 4).await; // num_send_bufs default
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn send_recv_multi_ring() {
    require_no_iwarp!();
    let config = CreditRingConfig::default();
    let max = config.ring_capacity / config.max_message_size;
    send_recv_multi(config, max).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn send_recv_multi_read_ring() {
    require_no_iwarp!();
    let config = ReadRingConfig::default();
    let max = config.ring_capacity / config.max_message_size;
    send_recv_multi(config, max).await;
}

// ===========================================================================
// backpressure_recovery — fill ring, repost, retry send.
// ===========================================================================

async fn backpressure_recovery<B: TransportBuilder>(config: B) {
    let (mut server, mut client) = ring_connected_pair(config).await;

    let data = vec![0xCDu8; 1500];
    let mut sent_count = 0;
    loop {
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_) => sent_count += 1,
            Err(e) => panic!("send_copy error: {e}"),
        }
    }
    assert!(sent_count > 0);
    drain_send_completions(&mut client).await;

    for _ in 0..3 {
        let rc = recv_one(&mut server).await;
        server.repost_recv(rc.buf_idx).unwrap();
    }

    let n = send_after_repost(&mut client, &data, "backpressure recovery").await;
    assert!(n > 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn backpressure_recovery_default() {
    backpressure_recovery(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn backpressure_recovery_ring() {
    require_no_iwarp!();
    backpressure_recovery(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn backpressure_recovery_read_ring() {
    require_no_iwarp!();
    backpressure_recovery(ReadRingConfig::default()).await;
}

// ===========================================================================
// wrap_around — fill, drain, resend to trigger ring wrap, verify integrity.
// ===========================================================================

async fn wrap_around<B: TransportBuilder>(config: B) {
    let (mut server, mut client) = ring_connected_pair(config).await;

    let msg_size = 1500;
    let mut sent_count = 0;
    loop {
        let data: Vec<u8> = (0..msg_size)
            .map(|i| ((sent_count * 7 + i) % 251) as u8)
            .collect();
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_) => sent_count += 1,
            Err(e) => panic!("send_copy error: {e}"),
        }
    }
    drain_send_completions(&mut client).await;

    for _ in 0..sent_count {
        let rc = recv_one(&mut server).await;
        server.repost_recv(rc.buf_idx).unwrap();
    }

    let wrap_data: Vec<u8> = (0..msg_size).map(|i| ((i + 42) % 253) as u8).collect();
    let n = send_after_repost(&mut client, &wrap_data, "wrap-around").await;
    assert!(n > 0);
    drain_send_completions(&mut client).await;

    let rc = recv_one(&mut server).await;
    assert_eq!(&server.recv_buf(rc.buf_idx)[..rc.byte_len], &wrap_data[..n]);
    server.repost_recv(rc.buf_idx).unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn wrap_around_default() {
    wrap_around(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn wrap_around_ring() {
    require_no_iwarp!();
    wrap_around(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn wrap_around_read_ring() {
    require_no_iwarp!();
    wrap_around(ReadRingConfig::default()).await;
}

// ===========================================================================
// disconnect — one side disconnects, verify poll_disconnect on the other.
// ===========================================================================

async fn disconnect<B: TransportBuilder>(config: B) {
    let (mut server, mut client) = ring_connected_pair(config).await;

    client.disconnect().unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        poll_fn(|cx| {
            if server.poll_disconnect(cx) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    })
    .await;

    assert!(result.is_ok(), "server should detect disconnect within 5s");
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn disconnect_default() {
    disconnect(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn disconnect_ring() {
    require_no_iwarp!();
    disconnect(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn disconnect_read_ring() {
    require_no_iwarp!();
    disconnect(ReadRingConfig::default()).await;
}

// ===========================================================================
// out_of_order_repost — repost in reverse order, verify send recovers.
// ===========================================================================

async fn out_of_order_repost<B: TransportBuilder>(config: B) {
    let (mut server, mut client) = ring_connected_pair(config).await;

    let data_a = b"message-A";
    let data_b = b"message-B";
    send_and_complete(&mut client, data_a).await;
    send_and_complete(&mut client, data_b).await;

    let rc_a = recv_one(&mut server).await;
    let rc_b = recv_one(&mut server).await;
    assert_eq!(&server.recv_buf(rc_a.buf_idx)[..rc_a.byte_len], data_a);
    assert_eq!(&server.recv_buf(rc_b.buf_idx)[..rc_b.byte_len], data_b);

    // Repost in reverse order
    server.repost_recv(rc_b.buf_idx).unwrap();
    server.repost_recv(rc_a.buf_idx).unwrap();

    let data_c = b"after-reorder";
    let n = send_after_repost(&mut client, data_c, "out-of-order repost").await;
    assert!(n > 0);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn out_of_order_repost_default() {
    out_of_order_repost(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn out_of_order_repost_ring() {
    require_no_iwarp!();
    out_of_order_repost(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn out_of_order_repost_read_ring() {
    require_no_iwarp!();
    out_of_order_repost(ReadRingConfig::default()).await;
}

// ===========================================================================
// virtual_idx_wrap — fill ring, drain, refill to recycle virtual idx slots.
// ===========================================================================

async fn virtual_idx_wrap<B: TransportBuilder>(config: B, max_outstanding: usize) {
    let (mut server, mut client) = ring_connected_pair(config).await;

    let data = vec![0xBBu8; 1500];

    // Batch 1: fill ring
    let mut batch1_count = 0;
    loop {
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_) => batch1_count += 1,
            Err(e) => panic!("send_copy error: {e}"),
        }
    }
    assert!(batch1_count > 0);
    drain_send_completions(&mut client).await;

    // Receive and repost all on server
    for _ in 0..batch1_count {
        let rc = recv_one(&mut server).await;
        assert_eq!(server.recv_buf(rc.buf_idx)[..rc.byte_len].len(), 1500);
        server.repost_recv(rc.buf_idx).unwrap();
    }
    drain_send_completions(&mut server).await;

    // Batch 2: virtual idx slots should be recycled.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    drain_send_completions(&mut client).await;

    let mut batch2_count = 0;
    for i in 0..max_outstanding {
        let msg: Vec<u8> = (0..1500).map(|j| ((i * 3 + j) % 251) as u8).collect();
        let n = loop {
            let mut completions = [RecvCompletion::default(); 8];
            let _ = poll_fn(|cx| match client.poll_recv(cx, &mut completions) {
                Poll::Ready(r) => Poll::Ready(Some(r)),
                Poll::Pending => Poll::Ready(None),
            })
            .await;
            drain_send_completions(&mut client).await;

            match client.send_copy(&msg) {
                Ok(0) if batch2_count > 0 => break 0,
                Ok(0) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "batch 2 should send messages (idx recycled) — timed out"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Ok(sent) => break sent,
                Err(e) => panic!("batch 2 send_copy error: {e}"),
            }
        };
        if n == 0 {
            break;
        }
        batch2_count += 1;
    }
    assert!(
        batch2_count > 0,
        "batch 2 should send messages (idx recycled)"
    );

    drain_send_completions(&mut client).await;
    for i in 0..batch2_count {
        let rc = recv_one(&mut server).await;
        let expected: Vec<u8> = (0..1500).map(|j| ((i * 3 + j) % 251) as u8).collect();
        assert_eq!(
            &server.recv_buf(rc.buf_idx)[..rc.byte_len],
            &expected[..],
            "batch 2 msg {i} integrity"
        );
        server.repost_recv(rc.buf_idx).unwrap();
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn virtual_idx_wrap_default() {
    let config = SendRecvConfig::datagram();
    virtual_idx_wrap(config, 4).await; // num_send_bufs default
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn virtual_idx_wrap_ring() {
    require_no_iwarp!();
    let config = CreditRingConfig::default();
    let max = config.ring_capacity / config.max_message_size;
    virtual_idx_wrap(config, max).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn virtual_idx_wrap_read_ring() {
    require_no_iwarp!();
    let config = ReadRingConfig::default();
    let max = config.ring_capacity / config.max_message_size;
    virtual_idx_wrap(config, max).await;
}

// ===========================================================================
// drop_safety — drop transports without disconnect, verify no panic.
// ===========================================================================

async fn drop_safety<B: TransportBuilder>(config: B) {
    let (_server, mut client) = ring_connected_pair(config).await;
    let _ = client.send_copy(b"drop-safety-test");
    let _ = client.send_copy(b"drop-safety-test");
    drop(client);
    drop(_server);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn drop_safety_default() {
    drop_safety(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn drop_safety_ring() {
    require_no_iwarp!();
    drop_safety(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn drop_safety_read_ring() {
    require_no_iwarp!();
    drop_safety(ReadRingConfig::default()).await;
}

// ===========================================================================
// multi_connection — two clients connect to same listener, echo independently.
// ===========================================================================

async fn multi_connection<B: TransportBuilder>(config: B) {
    let listener = AsyncCmListener::bind(&bind_addr()).unwrap();
    let connect_addr = connect_addr_for(listener.local_addr());
    let listener = std::sync::Arc::new(listener);

    let listener_s = listener.clone();
    let config_s = config.clone();
    let config_s2 = config.clone();
    let server_handle = tokio::spawn(async move {
        let t1 = config_s.accept(&listener_s).await.unwrap();
        let t2 = config_s2.accept(&listener_s).await.unwrap();
        (t1, t2)
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let config_c1 = config.clone();
    let addr1 = connect_addr;
    let client1 = tokio::spawn(async move { config_c1.connect(&addr1).await.unwrap() })
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let config_c2 = config;
    let addr2 = connect_addr;
    let client2_handle = tokio::spawn(async move { config_c2.connect(&addr2).await.unwrap() });

    let (server_res, c2_res) = tokio::join!(server_handle, client2_handle);
    let (mut server1, mut server2) = server_res.unwrap();
    let mut client1 = client1;
    let mut client2 = c2_res.unwrap();

    // Echo on connection 1
    let msg1 = b"hello from client 1";
    send_and_complete(&mut client1, msg1).await;
    let rc = recv_one(&mut server1).await;
    assert_eq!(&server1.recv_buf(rc.buf_idx)[..rc.byte_len], msg1);
    server1.repost_recv(rc.buf_idx).unwrap();

    // Echo on connection 2
    let msg2 = b"hello from client 2";
    send_and_complete(&mut client2, msg2).await;
    let rc = recv_one(&mut server2).await;
    assert_eq!(&server2.recv_buf(rc.buf_idx)[..rc.byte_len], msg2);
    server2.repost_recv(rc.buf_idx).unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn multi_connection_default() {
    multi_connection(SendRecvConfig::datagram()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn multi_connection_ring() {
    require_no_iwarp!();
    multi_connection(CreditRingConfig::default()).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn multi_connection_read_ring() {
    require_no_iwarp!();
    multi_connection(ReadRingConfig::default()).await;
}
