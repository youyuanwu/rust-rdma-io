//! ReadRingTransport-specific integration tests.
//!
//! Shared tests (connect/accept, send/recv, backpressure, wrap-around,
//! disconnect, out-of-order repost) live in `ring_common_tests.rs`.
//! This file retains only ReadRing-specific tests: the OOO repost
//! no-overwrite regression test.

use std::future::poll_fn;
use std::task::Poll;

use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use rdma_io::transport::{RecvCompletion, Transport};
use rdma_io::transport_common::MemoryWindowMode;
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::connect_addr_for;

/// Helper: create a connected (server, client) read-ring transport pair.
async fn ring_connected_pair(config: ReadRingConfig) -> (ReadRingTransport, ReadRingTransport) {
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());
    let config2 = config.clone();

    let server =
        tokio::spawn(async move { ReadRingTransport::accept(&listener, config2).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let client = tokio::spawn(async move {
        rdma_io_tests::test_helpers::connect_with_retry(&config, &connect_addr).await
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

    // Step 1: Fill the ring to byte-capacity. `send_copy` returns Ok(0) both
    // for a genuinely byte-full ring AND for transient send-queue backpressure
    // (outstanding Write+Imm is capped at max_outstanding so it never exceeds
    // the peer's doorbell pool). Draining send completions clears the transient
    // case and refreshes the RDMA-Read head; only a 0 that persists *after* a
    // full drain means the ring is actually byte-full.
    let mut sent_count = 0;
    let mut sent_data: Vec<Vec<u8>> = Vec::new();
    loop {
        let data: Vec<u8> = (0..msg_size)
            .map(|i| ((sent_count * 13 + i * 7 + 42) % 251) as u8)
            .collect();
        let mut n = match client.send_copy(&data) {
            Ok(n) => n,
            Err(e) => panic!("send_copy error: {e}"),
        };
        if n == 0 {
            drain_send_completions(&mut client).await;
            n = match client.send_copy(&data) {
                Ok(n) => n,
                Err(e) => panic!("send_copy error: {e}"),
            };
        }
        if n == 0 {
            break;
        }
        sent_data.push(data);
        sent_count += 1;
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

/// MR-rkey mode: with [`MemoryWindowMode::Disable`] the transport skips both
/// Memory Window binds (recv ring + offset buffer) and exchanges the MRs' own
/// rkeys. This path is used automatically on NICs that report `max_mw = 0`
/// (e.g. Azure MANA) and forced here via `Disable`; it also works on MW-capable
/// test NICs, so verify both the RDMA Write data path and the RDMA Read
/// flow-control path round-trip correctly.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_mr_rkey_fallback_roundtrip() {
    require_no_iwarp!();
    let config = ReadRingConfig::default().with_memory_window_mode(MemoryWindowMode::Disable);
    assert_eq!(
        config.mw_mode,
        MemoryWindowMode::Disable,
        "MR-rkey mode should be set"
    );
    let (mut server, mut client) = ring_connected_pair(config).await;

    // Send several messages, each with a distinct pattern, and verify they
    // arrive intact. send_after_credits drives the RDMA Read flow-control
    // path, which also relies on the offset-buffer MR rkey in this mode.
    for i in 0..8u8 {
        let data: Vec<u8> = (0..1500).map(|j| i.wrapping_add(j as u8)).collect();
        let n = send_after_credits(&mut client, &data, "mr-rkey fallback send").await;
        assert_eq!(n, data.len(), "full message accepted");
        drain_send_completions(&mut client).await;

        let rc = recv_one(&mut server).await;
        assert_eq!(
            &server.recv_buf(rc.buf_idx)[..rc.byte_len],
            &data[..],
            "message {i} data integrity over MR-rkey path"
        );
        server.repost_recv(rc.buf_idx).unwrap();
    }

    drop(client);
    drop(server);

    println!("read_ring_mr_rkey_fallback_roundtrip passed!");
}

/// Deterministic reproduction of the *root cause* behind the read-ring
/// concurrent-stream deadlock: a sender's **write completion is coupled to the
/// peer's read progress**.
///
/// Each `send_copy` posts an RDMA Write+Immediate that must be consumed by a
/// doorbell recv WR on the peer. The peer only reposts doorbells while draining
/// its recv ring (`poll_recv` / `repost_recv`) — i.e. on its *read* side. The
/// send queue (`max_outstanding + 3`) is deliberately deeper than the peer's
/// doorbell pool (`max_outstanding`), so a sender can post more Write+Imm than
/// the peer has doorbells. With small messages the 64 KiB byte ring never fills,
/// so the doorbell pool — not ring space — is the binding constraint; this is
/// exactly the SQ-backpressure path (`rif=false`) observed at the freeze.
///
/// Once the peer's doorbells are spent and it is not reading, the excess
/// Write+Imm RNR-stall and `send_in_flight` pins with no send-CQ wakeup. Over
/// gRPC both peers hit this symmetrically and neither ever reads again, so the
/// pin becomes a permanent deadlock. `send-recv` and `credit-ring` do not couple
/// write completion to peer read progress and pipeline cleanly.
///
/// This drives one direction to the wedge (peer never reads) and asserts:
///   1. `send_in_flight` pins > 0 and cannot drain despite a full send queue
///      (the deadlock condition), and
///   2. the peer reading + reposting doorbells is what unblocks the sender,
/// confirming the coupling.
///
/// NOTE: this documents *current* (buggy) behavior. A flow-control fix that
/// guarantees doorbell headroom ≥ send-queue depth would stop the wedge from
/// occurring; this test must then be revisited.
///
/// See docs/bugs/read-ring-concurrent-stream-deadlock.md.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_write_completion_coupled_to_peer_read() {
    require_no_iwarp!();
    // Default: max_outstanding = 43, doorbell pool = 43, send queue = 46.
    let (mut server, mut client) = ring_connected_pair(ReadRingConfig::default()).await;

    // Small messages so the byte ring never fills — the doorbell pool is the
    // binding constraint (matches the bug's SQ-backpressure path).
    let data = vec![0xABu8; 64];

    // Drive the client's send pipeline to the wedge WITHOUT the server ever
    // reading. Each round posts until backpressure (Ok(0)) then drains the send
    // CQ. A decoupled transport would let the peer HCA autonomously consume every
    // Write+Imm and complete these sends; here, once the 43 doorbells are spent,
    // further Write+Imm RNR-stall and cannot complete.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        while client.send_copy(&data).unwrap() > 0 {}
        drain_send_completions(&mut client).await;

        // Wedged when the send queue is full (Ok(0)) yet posted sends can't drain.
        if client.send_copy(&data).unwrap() == 0 && client.sends_in_flight() > 0 {
            let pinned = client.sends_in_flight();
            drain_send_completions(&mut client).await;
            if client.sends_in_flight() == pinned {
                break; // no send-CQ progress despite a full queue: the deadlock
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "did not reach the send-queue wedge within 10s"
        );
    }

    let pinned = client.sends_in_flight();
    assert!(
        pinned > 0,
        "expected posted sends pinned by the peer's exhausted doorbells"
    );
    println!("wedge reproduced: {pinned} sends pinned with the peer not reading");

    // The pin is NOT benign backpressure: it only clears once the *peer reads*.
    // Drain the server's recv ring and repost doorbells; each repost lets one
    // RNR-stalled Write+Imm complete on the client, cascading until all drain.
    let recover_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while client.sends_in_flight() > 0 {
        let mut completions = [RecvCompletion::default(); 8];
        let got = poll_fn(|cx| match server.poll_recv(cx, &mut completions) {
            Poll::Ready(r) => Poll::Ready(Some(r)),
            Poll::Pending => Poll::Ready(None),
        })
        .await;
        if let Some(Ok(n)) = got {
            for c in &completions[..n] {
                server.repost_recv(c.buf_idx).unwrap();
            }
        }
        drain_send_completions(&mut client).await;
        assert!(
            tokio::time::Instant::now() < recover_deadline,
            "client sends never drained even after the peer read ({} still pinned)",
            client.sends_in_flight()
        );
    }

    assert_eq!(
        client.sends_in_flight(),
        0,
        "peer read progress must unblock the sender's writes"
    );

    println!("read_ring_write_completion_coupled_to_peer_read passed!");
}

/// The doorbell-blocked backpressure path must post a one-sided RDMA-Read
/// *heartbeat* so a wedged sender still has a send-CQ wakeup source.
///
/// Unlike the byte-blocked path, the doorbell-blocked branch of `send_copy`
/// posts no data WR: the pinned Write+Imm are RNR-stalled on the peer's spent
/// doorbells (only the peer reading + reposting completes them), and nothing
/// else is outstanding. Without a proactive Read the caller would park on the
/// send CQ with NO completion ever arriving — the silent-park seed of the
/// concurrent-stream deadlock.
///
/// This asserts the *mechanism* deterministically (provider-independent): drive
/// `send_copy` into the doorbell-blocked branch (small messages, peer never
/// reads, so the 43-slot doorbell pool — not the 64 KiB byte ring — is the
/// binding constraint) and verify a Read is left in flight. The heartbeat is a
/// **liveness** guarantee, not a deadlock cure: it does not relieve the doorbell
/// block (only the peer reading does), it just keeps a send-CQ wakeup source
/// alive so the stream's recv-drain / doorbell-repost cascade cannot go silent.
///
/// (The *effect* — no silent park — is only observable cross-VM under real RTT;
/// on same-VM loopback data completions still trickle in, so it can't be
/// isolated in-process. Hence this checks the mechanism, not the effect.)
///
/// See docs/bugs/read-ring-concurrent-stream-deadlock.md.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_doorbell_blocked_posts_read_heartbeat() {
    require_no_iwarp!();
    // Default: max_outstanding = 43, doorbell pool = 43, 64 KiB ring. Keep the
    // server bound (idle, never reading) so the connection stays up.
    let (mut _server, mut client) = ring_connected_pair(ReadRingConfig::default()).await;

    // Small messages so the doorbell pool (not byte space) is the binding
    // constraint — the SQ-backpressure path the heartbeat protects. Post WITHOUT
    // draining and without the server reading, so the send queue fills to the
    // doorbell cap and the next send_copy takes the doorbell-blocked branch.
    let data = vec![0xABu8; 64];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if client.send_copy(&data).unwrap() == 0 && client.sends_in_flight() > 0 {
            break; // doorbell-blocked: Ok(0) with sends pinned
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "did not reach doorbell backpressure within 10s"
        );
    }

    // Tiny messages (64 B) can only fill ~43 * 64 B = 2.7 KiB of the 64 KiB ring,
    // so this is unambiguously the doorbell-pool constraint, not byte-fill.
    let pinned = client.sends_in_flight();
    assert!(
        pinned >= 40,
        "expected the doorbell pool (~43) to be the binding constraint, not byte \
         space; got only {pinned} sends pinned"
    );

    // The heartbeat: a doorbell-blocked send_copy must leave a one-sided RDMA
    // Read outstanding so the wedged sender retains a send-CQ wakeup source.
    assert!(
        client.read_in_flight(),
        "doorbell-blocked send_copy must post an RDMA-Read heartbeat; without it a \
         wedged sender parks with no send-CQ wakeup source (the concurrent-stream \
         deadlock's silent-park seed). \
         See docs/bugs/read-ring-concurrent-stream-deadlock.md"
    );

    println!("read_ring_doorbell_blocked_posts_read_heartbeat passed!");
}

/// Regression test for the send-CQ ownership invariant.
///
/// `poll_send_completion` (driven by the `CqPollState` drain-after-arm state
/// machine) must be the **sole** consumer of the send CQ. `send_copy` must
/// never reap send completions itself: a second, out-of-band poll of the send
/// CQ (the old `drain_send_cq_for_read`) races the arming state machine and can
/// swallow the completion notification `poll_send_completion` is parked on, so a
/// write-blocked writer whose sends have already completed never wakes. That is
/// the residual sustained-load stall — a lost send-CQ wakeup — behind the MANA
/// `pipelined_transfer_read_ring` flake.
///
/// The invariant is testable deterministically without depending on the race:
/// `send_copy` only ever *posts* work (incrementing `sends_in_flight()`); it
/// must **never decrease** `sends_in_flight()`. Any decrease across a bare
/// `send_copy` call (with no intervening `poll_send_completion`) means
/// `send_copy` reaped the send CQ behind the state machine's back.
///
/// See docs/bugs/read-ring-concurrent-stream-deadlock.md.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn read_ring_send_copy_must_not_reap_send_cq() {
    require_no_iwarp!();

    // Large doorbell pool + tiny byte ring. Two effects make this the exact
    // state that exposes an out-of-band send-CQ reap:
    //   * the byte ring (not the doorbell pool) is the binding constraint, so
    //     `send_copy` posts a proactive RDMA-Read once free space drops below
    //     threshold — `read_in_flight` becomes true, arming the buggy
    //     `drain_send_cq_for_read` path at the top of `send_copy`; and
    //   * with ~40 doorbells pre-posted, every Write+Imm is caught immediately
    //     and completes at HW, so completed-but-unreaped completions accumulate
    //     in the client's send CQ — exactly what a stray reap would consume.
    let config = ReadRingConfig {
        ring_capacity: 8192,
        max_message_size: 1024,
        max_in_flight: Some(40),
        min_free_threshold: 512,
        ..ReadRingConfig::default()
    };
    let (mut server, mut client) = ring_connected_pair(config).await;

    let data = vec![0xABu8; 1024];

    // Cycle the ring for a few seconds: fill it, let the HW complete the posted
    // Write+Imm, drain them *only* through the legitimate owner
    // (`poll_send_completion`), let the server read to free byte space, repeat.
    // Around EVERY bare `send_copy` call, assert it never reduced
    // `sends_in_flight()`.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut posted_total = 0usize;
    loop {
        let before = client.sends_in_flight();
        let n = client.send_copy(&data).unwrap();
        let after = client.sends_in_flight();
        assert!(
            after >= before,
            "send_copy reduced sends_in_flight {before} -> {after}: it reaped the \
             send CQ behind poll_send_completion's back (the lost-wakeup bug). \
             poll_send_completion must be the sole consumer of the send CQ. \
             See docs/bugs/read-ring-concurrent-stream-deadlock.md"
        );

        if n > 0 {
            posted_total += 1;
            continue;
        }

        // Ring full (`Ok(0)`): let the HW finish the in-flight Write+Imm, then
        // reap them through the legitimate owner only, and let the server read
        // to free byte space so the next iteration keeps posting.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        drain_send_completions(&mut client).await;

        let mut completions = [RecvCompletion::default(); 16];
        if let Poll::Ready(Ok(got)) = poll_fn(|cx| match server.poll_recv(cx, &mut completions) {
            Poll::Ready(r) => Poll::Ready(Poll::Ready(r)),
            Poll::Pending => Poll::Ready(Poll::Pending),
        })
        .await
        {
            for c in &completions[..got] {
                server.repost_recv(c.buf_idx).unwrap();
            }
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    assert!(
        posted_total > 0,
        "test did not post any sends — configuration did not exercise the ring"
    );

    println!("read_ring_send_copy_must_not_reap_send_cq passed! (posted {posted_total})");
}
