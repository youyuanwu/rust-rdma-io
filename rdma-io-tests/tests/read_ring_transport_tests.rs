//! ReadRingTransport integration tests.
//!
//! Tests verify that `ReadRingTransport` provides datagram-style RDMA Write
//! over ring buffers with Memory Window scoping, RDMA Read-based flow control
//! (chase-forward offset buffer), and wrap-around handling.
//!
//! Mirrors the P0 tests from `ring_transport_tests.rs` but adapted for the
//! ReadRing flow-control model: the sender discovers freed space by RDMA
//! Reading the remote offset buffer, rather than receiving credit Send+Imm WRs.

use std::future::poll_fn;
use std::task::Poll;

use rdma_io::async_cm::AsyncCmListener;
use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use rdma_io::transport::{RecvCompletion, Transport};
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::{bind_addr, connect_addr_for};

/// Helper: create a connected (server, client) read-ring transport pair.
async fn ring_connected_pair(config: ReadRingConfig) -> (ReadRingTransport, ReadRingTransport) {
    let listener = AsyncCmListener::bind(&bind_addr()).unwrap();
    let connect_addr = connect_addr_for(listener.local_addr());
    let config2 = config.clone();

    let server =
        tokio::spawn(async move { ReadRingTransport::accept(&listener, config2).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let client = tokio::spawn(async move {
        ReadRingTransport::connect(&connect_addr, config)
            .await
            .unwrap()
    });
    let (s, c) = tokio::join!(server, client);
    (s.unwrap(), c.unwrap())
}

/// Helper: send data and wait for send completion.
async fn send_and_complete(transport: &mut ReadRingTransport, data: &[u8]) -> usize {
    let n = transport.send_copy(data).unwrap();
    assert!(n > 0, "send_copy returned 0 — no space in ring");
    poll_fn(|cx| transport.poll_send_completion(cx))
        .await
        .unwrap();
    n
}

/// Helper: drain all pending send completions (non-blocking).
async fn drain_send_completions(transport: &mut ReadRingTransport) {
    while let Some(Ok(())) = poll_fn(|cx| match transport.poll_send_completion(cx) {
        Poll::Ready(r) => Poll::Ready(Some(r)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
    {}
}

/// Helper: send data, retrying until the offset buffer reflects freed space.
///
/// ReadRing sender uses RDMA Read internally — when `send_copy` returns
/// Ok(0), it posts an RDMA Read of the remote offset buffer. On the next
/// call it picks up the Read result and recalculates free space. We just
/// retry `send_copy` in a loop with sleep.
async fn send_after_credits(transport: &mut ReadRingTransport, data: &[u8], ctx: &str) -> usize {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
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

/// Helper: receive one data completion.
///
/// ReadRing's `poll_recv` internally loops past padding-only batches,
/// so it should never return Ok(0). We still guard against unexpected
/// zero-count returns with a retry loop.
async fn recv_one(transport: &mut ReadRingTransport) -> RecvCompletion {
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

// ===========================================================================
// P0 — Core Functionality
// ===========================================================================

/// P0: Connect + accept, verify both sides completed without error.
/// Verify local_addr/peer_addr return Some.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_connect_accept() {
    require_no_iwarp!();
    let (server, client) = ring_connected_pair(ReadRingConfig::default()).await;

    assert!(
        server.local_addr().is_some(),
        "server local_addr should be Some"
    );
    assert!(
        server.peer_addr().is_some(),
        "server peer_addr should be Some"
    );
    assert!(
        client.local_addr().is_some(),
        "client local_addr should be Some"
    );
    assert!(
        client.peer_addr().is_some(),
        "client peer_addr should be Some"
    );

    println!("read_ring_connect_accept passed!");
}

/// P0: Send 1500 bytes from client → server. Verify recv_buf returns exact data.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_send_recv_single() {
    require_no_iwarp!();
    let (mut server, mut client) = ring_connected_pair(ReadRingConfig::default()).await;

    let data: Vec<u8> = (0..1500).map(|i| (i % 251) as u8).collect();
    send_and_complete(&mut client, &data).await;

    let rc = recv_one(&mut server).await;
    assert_eq!(rc.byte_len, 1500);

    let received = server.recv_buf(rc.buf_idx);
    assert!(
        !received.is_empty(),
        "recv_buf should return non-empty slice"
    );
    assert_eq!(received, &data[..], "received data must match sent data");

    server.repost_recv(rc.buf_idx).unwrap();

    println!("read_ring_send_recv_single passed!");
}

/// P0: Send datagrams until send_copy returns Ok(0) (ring full / backpressure).
/// With default config (64KB ring / 1500B msg = 43 max_outstanding),
/// sending 43+ should trigger backpressure.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_send_recv_multi() {
    require_no_iwarp!();
    let config = ReadRingConfig::default();
    let max_outstanding = config.ring_capacity / config.max_message_size;
    let (mut _server, mut client) = ring_connected_pair(config).await;

    let data = vec![0xABu8; 1500];
    let mut sent_count = 0;
    for _ in 0..(max_outstanding + 5) {
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_n) => {
                sent_count += 1;
            }
            Err(e) => panic!("unexpected send_copy error: {e}"),
        }
    }

    assert!(
        sent_count <= max_outstanding,
        "sent {sent_count} messages but max_outstanding is {max_outstanding}"
    );
    assert!(sent_count > 0, "should have sent at least one message");

    // Now send_copy should return Ok(0) since the ring is full
    let n = client.send_copy(&data).unwrap();
    assert_eq!(n, 0, "should be backpressured after ring full");

    println!("read_ring_send_recv_multi passed! (sent {sent_count} before backpressure)");
}

/// P0: After backpressure, receiver reposts buffers (advancing the offset
/// buffer), sender retries send_copy (which internally RDMA Reads the
/// remote offset buffer to discover freed space).
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_credit_recovery() {
    require_no_iwarp!();
    let config = ReadRingConfig::default();
    let (mut server, mut client) = ring_connected_pair(config).await;

    // Send until ring is full
    let data = vec![0xCDu8; 1500];
    let mut sent_count = 0;
    loop {
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_) => sent_count += 1,
            Err(e) => panic!("send_copy error: {e}"),
        }
    }
    assert!(sent_count > 0, "should have sent some messages");

    // Drain all send completions so the send ring has space
    drain_send_completions(&mut client).await;

    // Receiver: recv and repost a few messages to advance offset buffer
    let repost_count = 3;
    for _ in 0..repost_count {
        let rc = recv_one(&mut server).await;
        server.repost_recv(rc.buf_idx).unwrap();
    }

    // Sender retries send_copy — internal RDMA Read picks up the new offset.
    let n = send_after_credits(&mut client, &data, "credit recovery").await;
    assert!(n > 0, "send_copy should succeed after credit recovery");

    println!("read_ring_credit_recovery passed!");
}

/// P0: Send messages that cause the ring tail to wrap around. With 1500B
/// messages in a 64KB ring, after ~43 messages + repost + re-send, the
/// ring should wrap. Verify data integrity after wrap.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_wrap_around() {
    require_no_iwarp!();
    let config = ReadRingConfig::default();
    let (mut server, mut client) = ring_connected_pair(config).await;

    let msg_size = 1500;
    // First batch: fill until ring is full
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

    // Drain all pending send completions
    drain_send_completions(&mut client).await;

    // Receive and repost all on server side
    for _ in 0..sent_count {
        let rc = recv_one(&mut server).await;
        server.repost_recv(rc.buf_idx).unwrap();
    }

    // Sender retries until offset buffer reflects freed space.
    let wrap_data: Vec<u8> = (0..msg_size).map(|i| ((i + 42) % 253) as u8).collect();
    let n = send_after_credits(&mut client, &wrap_data, "wrap-around").await;
    assert!(n > 0, "send should succeed after credits recovered (wrap)");
    drain_send_completions(&mut client).await;

    let rc = recv_one(&mut server).await;
    let received = server.recv_buf(rc.buf_idx);
    assert_eq!(
        received,
        &wrap_data[..n],
        "data integrity after wrap-around"
    );

    server.repost_recv(rc.buf_idx).unwrap();

    println!("read_ring_wrap_around passed!");
}

/// P0: One side calls disconnect(), verify the other side's poll_disconnect
/// returns true within 5 seconds.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_disconnect() {
    require_no_iwarp!();
    let (mut server, mut client) = ring_connected_pair(ReadRingConfig::default()).await;

    // Client disconnects
    client.disconnect().unwrap();

    // Server should detect disconnect within 5 seconds
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

    println!("read_ring_disconnect passed!");
}

/// P1: Receive 2 messages (buf_idx A and B). Repost B first, then A.
/// Verify both succeed and the sender recovers (offset buffer advances
/// once both contiguous slots are released).
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_out_of_order_repost() {
    require_no_iwarp!();
    let (mut server, mut client) = ring_connected_pair(ReadRingConfig::default()).await;

    // Send 2 messages
    let data_a = b"message-A";
    let data_b = b"message-B";
    send_and_complete(&mut client, data_a).await;
    send_and_complete(&mut client, data_b).await;

    // Receive both
    let rc_a = recv_one(&mut server).await;
    let rc_b = recv_one(&mut server).await;

    assert_eq!(server.recv_buf(rc_a.buf_idx), data_a);
    assert_eq!(server.recv_buf(rc_b.buf_idx), data_b);

    // Repost in reverse order: B first, then A
    server.repost_recv(rc_b.buf_idx).unwrap();
    server.repost_recv(rc_a.buf_idx).unwrap();

    // Verify sender recovers — retry since RDMA Read is async.
    let data_c = b"after-reorder";
    let n = send_after_credits(&mut client, data_c, "out-of-order repost").await;
    assert!(n > 0, "send should work after out-of-order repost");

    println!("read_ring_out_of_order_repost passed!");
}

/// Regression test: Out-of-order repost_recv must NOT cause the sender to
/// overwrite unreleased data in the recv ring.
///
/// ReadRing uses a chase-forward offset buffer with CompletionTracker —
/// the offset buffer only advances when contiguous slots are released.
/// Holding slot 0 prevents the sender from seeing any freed space, even
/// if all other slots are released. This test verifies that invariant.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_ooo_repost_no_overwrite() {
    require_no_iwarp!();
    let config = ReadRingConfig::default(); // 64KB ring, 1500B msgs, max_outstanding=43
    let (mut server, mut client) = ring_connected_pair(config).await;

    let msg_size = 1500;

    // Step 1: Fill ring completely — send until backpressure.
    let mut sent_count = 0;
    let mut sent_data: Vec<Vec<u8>> = Vec::new();
    loop {
        let data: Vec<u8> = (0..msg_size)
            .map(|i| ((sent_count * 13 + i * 7 + 42) % 251) as u8)
            .collect();
        match client.send_copy(&data) {
            Ok(0) => break,
            Ok(_) => {
                sent_data.push(data);
                sent_count += 1;
            }
            Err(e) => panic!("send_copy error: {e}"),
        }
    }
    assert!(sent_count >= 3, "need at least 3 messages to test OOO");
    println!("sent {sent_count} messages, ring full");

    // Drain all send completions.
    drain_send_completions(&mut client).await;

    // Step 2: Receive ALL messages on server.
    let mut recv_completions = Vec::new();
    for _ in 0..sent_count {
        let rc = recv_one(&mut server).await;
        recv_completions.push(rc);
    }

    // Verify message 0 data is correct before any repost.
    let msg0_rc = recv_completions[0];
    let msg0_original = server.recv_buf(msg0_rc.buf_idx)[..msg0_rc.byte_len].to_vec();
    assert_eq!(
        msg0_original, sent_data[0],
        "message 0 should match before any repost"
    );

    // Step 3: Repost all messages EXCEPT message 0 (out-of-order release).
    for rc in &recv_completions[1..] {
        server.repost_recv(rc.buf_idx).unwrap();
    }
    println!("reposted messages 1..{sent_count}, holding message 0");

    // Step 4: Try to send — sender should get NO credits because slot 0
    // blocks contiguous advance in the CompletionTracker.
    // The offset buffer hasn't moved, so the sender sees no freed space.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drain_send_completions(&mut client).await;

    let new_pattern: Vec<u8> = (0..msg_size)
        .map(|i| (0xBB_u8).wrapping_add(i as u8))
        .collect();
    let n = client.send_copy(&new_pattern).unwrap();
    assert_eq!(
        n, 0,
        "sender should have 0 credits — slot 0 blocks contiguous advance"
    );
    println!("confirmed: sender blocked (0 credits) while msg 0 unreleased");

    // Step 5: Verify message 0 data is intact (NOT overwritten).
    // ReadRing uses chase-forward offset buffer, so this SHOULD pass.
    let msg0_after = server.recv_buf(msg0_rc.buf_idx)[..msg0_rc.byte_len].to_vec();
    assert_eq!(
        msg0_after, msg0_original,
        "message 0 data should be intact — chase-forward offset buffer prevents overwrite"
    );

    // Step 6: Release message 0 — offset buffer should now advance
    // past all contiguous released slots (0..N).
    server.repost_recv(msg0_rc.buf_idx).unwrap();
    println!("released message 0 — offset buffer should advance");

    // Step 7: Sender should now be able to send (all space recovered at once).
    let n = send_after_credits(&mut client, &new_pattern, "post-flush send").await;
    assert!(n > 0, "sender should have space after slot 0 released");
    drain_send_completions(&mut client).await;

    // Verify the new message arrived intact on the server.
    let rc = recv_one(&mut server).await;
    assert_eq!(
        &server.recv_buf(rc.buf_idx)[..rc.byte_len],
        &new_pattern[..n],
        "new message data integrity after OOO recovery"
    );
    server.repost_recv(rc.buf_idx).unwrap();

    println!("read_ring_ooo_repost_no_overwrite passed!");
}
