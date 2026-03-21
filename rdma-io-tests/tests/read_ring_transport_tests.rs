//! ReadRingTransport-specific integration tests.
//!
//! Shared tests (connect/accept, send/recv, backpressure, wrap-around,
//! disconnect, out-of-order repost) live in `ring_common_tests.rs`.
//! This file retains only ReadRing-specific tests: the OOO repost
//! no-overwrite regression test.

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
// ReadRing-specific tests
// ===========================================================================

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
