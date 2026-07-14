//! Busy-poll (`CoreDriver`) read-ring integration test.
//!
//! Proves the read-ring **data path** end-to-end when completions are produced
//! by the per-core shared-CQ [`CoreDriver`] instead of per-connection arm-park
//! CQs: one driver on one (pinned) `current_thread` runtime serves both ends of
//! a loopback connection, demuxing by `qp_num` into each connection's inbox.
//!
//! Also exercises the Slice C teardown barrier: dropping a busy transport hands
//! its resources to the driver's reclaim queue, and the driver drains the flush
//! CQEs to zero, frees the resources, and retires the `qp_num` before the
//! shutdown join returns. The churn test additionally stresses the
//! `qp_num`-retirement (TIME_WAIT) reuse guard across rapid reconnects.

use std::future::poll_fn;
use std::task::Poll;

use rdma_io::async_cm::AsyncCmId;
use rdma_io::cm::PortSpace;
use rdma_io::core_driver::CoreDriver;
use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use rdma_io::transport::{RecvCompletion, Transport};
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::{bind_listener_with_retry, connect_addr_for};

/// Drain all currently-available send completions (non-blocking).
async fn drain_sends(t: &mut ReadRingTransport) {
    while let Some(Ok(())) = poll_fn(|cx| match t.poll_send_completion(cx) {
        Poll::Ready(r) => Poll::Ready(Some(r)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
    {}
}

/// Send one message, retrying against read-ring backpressure.
async fn send_msg(t: &mut ReadRingTransport, data: &[u8], ctx: &str) {
    for _ in 0..100_000 {
        drain_sends(t).await;
        if t.send_copy(data).unwrap() > 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{ctx}: send_copy never accepted the message");
}

/// Receive exactly one message and return its bytes (reposts the recv buffer).
async fn recv_msg(t: &mut ReadRingTransport, ctx: &str) -> Vec<u8> {
    let mut c = [RecvCompletion::default(); 1];
    for _ in 0..100_000 {
        let n = poll_fn(|cx| t.poll_recv(cx, &mut c)).await.unwrap();
        if n > 0 {
            let data = t.recv_buf(c[0].buf_idx)[..c[0].byte_len].to_vec();
            t.repost_recv(c[0].buf_idx).unwrap();
            return data;
        }
    }
    panic!("{ctx}: poll_recv never returned data");
}

fn payload(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((seed * 31 + i * 7 + 13) % 251) as u8)
        .collect()
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn read_ring_busy_echo_bidirectional() {
    require_no_iwarp!();

    let config = ReadRingConfig::default();

    // Bind the listener and pick the loopback connect address (the local RDMA
    // IP on MANA / rxe).
    let listener = bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    // Obtain the shared per-device verbs context for the driver's shared CQs.
    // librdmacm caches one ibv_context per device across cm_ids, so QPs created
    // by connect_busy/accept_busy (their own cm_ids on the same device) can be
    // built against these CQs. Keep `probe` alive so the context stays valid.
    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    // One driver, shared CQs sized generously for two loopback connections.
    let (driver, handle) = CoreDriver::new(ctx, 1024, 1024).unwrap();
    let driver_task = tokio::spawn(driver.run());

    // Establish the connection pair in busy mode over the one driver.
    let server = {
        let h = handle.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            ReadRingTransport::accept_busy(&listener, cfg, &h)
                .await
                .unwrap()
        })
    };
    tokio::task::yield_now().await;
    let client = {
        let h = handle.clone();
        let addr = connect_addr;
        tokio::spawn(async move {
            ReadRingTransport::connect_busy(&addr, config, &h)
                .await
                .unwrap()
        })
    };
    let (server, client) = tokio::join!(server, client);
    let mut server = server.unwrap();
    let mut client = client.unwrap();

    // Client -> server.
    let n_msgs = 16;
    let msg_len = 256;
    for i in 0..n_msgs {
        let data = payload(i, msg_len);
        send_msg(&mut client, &data, "client->server send").await;
        let got = recv_msg(&mut server, "client->server recv").await;
        assert_eq!(got, data, "client->server payload mismatch at {i}");
    }

    // Server -> client (exercises the other direction's inboxes).
    for i in 0..n_msgs {
        let data = payload(i + 1000, msg_len);
        send_msg(&mut server, &data, "server->client send").await;
        let got = recv_msg(&mut client, "server->client recv").await;
        assert_eq!(got, data, "server->client payload mismatch at {i}");
    }

    // No completion should ever have hit an unknown qp_num (§4.2).
    assert_eq!(
        handle.unknown_qp_count(),
        0,
        "driver routed a completion to an unknown qp_num"
    );

    // Teardown (Slice C reclaim barrier): drop the transports FIRST so each busy
    // `Drop` hands its resources to the driver's reclaim queue (forcing the QP to
    // ERR up front). Then ask the driver to stop: its run loop keeps sweeping +
    // reclaiming until every handed-off bundle has drained to zero, been freed,
    // and had its `qp_num` retired — only then does the task return. A clean
    // `Ok(())` join is the assertion that the barrier completed without wedging
    // or a WR-accounting underflow panic.
    drop(client);
    drop(server);
    handle.shutdown();
    driver_task
        .await
        .expect("driver task panicked (barrier underflow or wedge)");

    assert_eq!(
        handle.unknown_qp_count(),
        0,
        "driver routed a completion to an unknown qp_num during teardown"
    );
    drop(probe);
}

/// Rapid connect → echo → drop churn on a single driver: exercises the Slice C
/// reclaim barrier and the `qp_num`-retirement (TIME_WAIT) reuse guard. Each
/// dropped transport hands its resources to the driver's reclaim queue; the
/// driver must drain, free, and retire each before its `qp_num` can be reused,
/// with no completion ever mis-routed to a stale/reused slot.
#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn read_ring_busy_reconnect_churn() {
    require_no_iwarp!();

    let config = ReadRingConfig::default();
    let listener = bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    let (driver, handle) = CoreDriver::new(ctx, 1024, 1024).unwrap();
    let driver_task = tokio::spawn(driver.run());

    // Each cycle establishes a fresh pair over the SAME driver/shared CQs, echoes
    // one message, then drops both ends. The two dropped bundles must be fully
    // reclaimed (drained + freed + retired) before their QPs/`qp_num`s are reused
    // by a later cycle — the churn-critical path.
    for cycle in 0..24usize {
        // Concurrently accept + connect on this task (no spawn — the listener is
        // reused across cycles, so it cannot be moved into a task).
        let accept_fut = ReadRingTransport::accept_busy(&listener, config.clone(), &handle);
        let connect_fut = ReadRingTransport::connect_busy(&connect_addr, config.clone(), &handle);
        let (server, client) = tokio::join!(accept_fut, connect_fut);
        let mut server = server.unwrap();
        let mut client = client.unwrap();

        let data = payload(cycle, 128);
        send_msg(&mut client, &data, "churn client->server send").await;
        let got = recv_msg(&mut server, "churn client->server recv").await;
        assert_eq!(got, data, "payload mismatch at cycle {cycle}");

        // Drop both ends → two reclaim handoffs. Yield repeatedly so the driver
        // reaps the flush CQEs and retires both `qp_num`s before the next cycle.
        drop(client);
        drop(server);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            handle.unknown_qp_count(),
            0,
            "driver mis-routed a completion (stale/reused qp_num) at cycle {cycle}"
        );
    }

    handle.shutdown();
    driver_task
        .await
        .expect("driver task panicked during churn reclaim");
    assert_eq!(handle.unknown_qp_count(), 0);
    drop(probe);
}
