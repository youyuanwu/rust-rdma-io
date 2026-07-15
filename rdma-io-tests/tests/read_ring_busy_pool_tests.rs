//! Busy-poll `BusyPool` multi-connection, multi-core test — Phase 1 Slice D1.
//!
//! Proves the harness-layer per-core pool: N busy-poll read-ring **clients**
//! sharded round-robin across M pinned `current_thread` cores (each with its own
//! [`CoreDriver`](rdma_io::core_driver::CoreDriver)), echoing against a single
//! arm-park server. Each client's `connect_busy` setup *and* its echo loop run
//! on its owning core (co-location), and teardown follows the reclaim protocol:
//! every client `JoinHandle` is awaited (each transport `Drop` hands off to its
//! core's reclaim queue) before the pool is shut down (drivers drain, threads
//! join).
//!
//! Busy client ↔ arm-park server exercises that the completion *mode* is local:
//! the read-ring wire protocol is identical, so the modes interoperate.

use std::future::poll_fn;
use std::sync::{Arc, LazyLock};
use std::task::Poll;

use rdma_io::async_cm::AsyncCmId;
use rdma_io::cm::PortSpace;
use rdma_io::read_ring_transport::{ReadRingConfig, ReadRingTransport};
use rdma_io::transport::{RecvCompletion, Transport};
use rdma_io_busy::BusyPool;
use rdma_io_tests::require_no_iwarp;
use rdma_io_tests::test_helpers::{bind_listener_with_retry, connect_addr_for};

/// Busy-poll pools pin OS threads to specific cores and spin them at 100%. These
/// loopback tests also run the *peer* — and, on a software transport like rxe,
/// the data path itself — in the same process, so two busy-pool tests running at
/// once would pin overlapping cores and starve each other (and the co-located
/// work) into a livelock. libtest runs tests in parallel by default, so
/// serialize the busy-pool tests through this lock. (The ansible runner also
/// passes `--test-threads=1`, but a direct `cargo test` does not.)
static BUSY_POOL_SERIAL: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Acquire the busy-pool serialization lock and pick the core ids for `cores`
/// pinned pollers — but only if the host has enough CPUs to dedicate them while
/// leaving headroom for the co-located peer and (on rxe) the software data path.
/// Returns `None` to skip on an under-provisioned host (e.g. a small rxe VM),
/// where pinned 100%-spin pollers would starve the loopback peer into a hang.
async fn busy_pool_guard(
    cores: usize,
) -> Option<(tokio::sync::MutexGuard<'static, ()>, Vec<usize>)> {
    let guard = BUSY_POOL_SERIAL.lock().await;
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let needed = cores + 2; // pinned pollers + headroom for peer/runtime/kernel
    if available < needed {
        eprintln!(
            "skipping busy-pool test: needs >= {needed} CPUs ({cores} pinned pollers + headroom), \
             host has {available}"
        );
        return None;
    }
    Some((guard, (0..cores).collect()))
}

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

/// Arm-park server side: echo exactly `count` messages, then disconnect.
async fn server_echo_loop(mut t: ReadRingTransport, count: usize) {
    for _ in 0..count {
        let data = recv_msg(&mut t, "server recv").await;
        send_msg(&mut t, &data, "server echo send").await;
    }
    let _ = t.disconnect();
}

/// Arm-park server side: echo every received message back until the peer
/// disconnects (used by the mixed-load / disconnect-isolation tests, where each
/// client sends a different, unknown number of messages before closing).
async fn server_echo_until_close(mut t: ReadRingTransport) {
    loop {
        let mut c = [RecvCompletion::default(); 1];
        // `Some(Some(data))` = a message to echo; `Some(None)` = progress with no
        // message; `None` = the peer closed (disconnect or fatal completion).
        let step = poll_fn(|cx| match t.poll_recv(cx, &mut c) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    let data = t.recv_buf(c[0].buf_idx)[..c[0].byte_len].to_vec();
                    t.repost_recv(c[0].buf_idx).unwrap();
                    Poll::Ready(Some(Some(data)))
                } else {
                    Poll::Ready(Some(None))
                }
            }
            Poll::Ready(Err(_)) => Poll::Ready(None),
            Poll::Pending => {
                if t.poll_disconnect(cx) {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
        })
        .await;
        match step {
            Some(Some(data)) => send_msg(&mut t, &data, "server echo send").await,
            Some(None) => {}
            None => break,
        }
    }
    let _ = t.disconnect();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_ring_busy_pool_multi_core_echo() {
    require_no_iwarp!();

    const CORES: usize = 2;
    const CONNS: usize = 6;
    const MSGS: usize = 8;
    const MSG_LEN: usize = 256;

    let Some((_serial, core_ids)) = busy_pool_guard(CORES).await else {
        return;
    };

    let config = ReadRingConfig::default();
    let listener = bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    // Probe for the shared per-device verbs context that backs the pool's
    // per-core driver CQs (librdmacm caches one ibv_context per device). Keep
    // `probe` alive for the pool's lifetime.
    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    // Arm-park echo server: accept CONNS connections, echo MSGS on each.
    let server_cfg = config.clone();
    let server = tokio::spawn(async move {
        let mut conns = Vec::with_capacity(CONNS);
        for _ in 0..CONNS {
            let t = ReadRingTransport::accept(&listener, server_cfg.clone())
                .await
                .expect("server accept");
            conns.push(tokio::spawn(server_echo_loop(t, MSGS)));
        }
        for h in conns {
            h.await.expect("server echo task panicked");
        }
    });

    // Busy-poll client pool across CORES pinned cores (ids from the guard).
    let pool = BusyPool::new(ctx, &core_ids, 1024, 1024).expect("build busy pool");
    assert_eq!(pool.core_count(), CORES);

    // Shard CONNS clients round-robin across the pool; each echoes on its core.
    let mut clients = Vec::with_capacity(CONNS);
    for i in 0..CONNS {
        let cfg = config.clone();
        clients.push(
            pool.spawn_connect(connect_addr, cfg, move |mut t| async move {
                for m in 0..MSGS {
                    let data = payload(i * 100 + m, MSG_LEN);
                    send_msg(&mut t, &data, "pool client send").await;
                    let got = recv_msg(&mut t, "pool client recv").await;
                    assert_eq!(got, data, "conn {i} msg {m} payload mismatch");
                }
            })
            .expect("uncapped pool always admits"),
        );
    }

    // Await every client: each transport `Drop` (when its closure returns) hands
    // its resources to the owning core's reclaim queue.
    for (i, h) in clients.into_iter().enumerate() {
        h.await
            .unwrap_or_else(|e| panic!("client {i} task panicked: {e}"))
            .unwrap_or_else(|e| panic!("client {i} connect_busy failed: {e}"));
    }
    server.await.expect("server task panicked");

    // Clean shutdown-join: stop each driver, drain its reclaim queue, join the
    // pinned threads. A hang here would mean a wedged reclaim barrier.
    pool.shutdown();
    drop(probe);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_ring_busy_pool_server_echo() {
    require_no_iwarp!();

    const CORES: usize = 2;
    const CONNS: usize = 6;
    const MSGS: usize = 8;
    const MSG_LEN: usize = 256;

    let Some((_serial, core_ids)) = busy_pool_guard(CORES).await else {
        return;
    };

    let config = ReadRingConfig::default();
    let listener = Arc::new(bind_listener_with_retry().await);
    let connect_addr = connect_addr_for(listener.local_addr());

    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    let pool = BusyPool::new(ctx, &core_ids, 1024, 1024).expect("build busy pool");

    // Arm-park clients connect + echo. Spawned first so they are connecting while
    // the busy pool server accepts (the handshake is serialized server-side).
    let mut clients = Vec::with_capacity(CONNS);
    for i in 0..CONNS {
        let addr = connect_addr;
        let cfg = config.clone();
        clients.push(tokio::spawn(async move {
            let mut t = ReadRingTransport::connect(&addr, cfg)
                .await
                .expect("client connect");
            for m in 0..MSGS {
                let data = payload(i * 100 + m, MSG_LEN);
                send_msg(&mut t, &data, "client send").await;
                let got = recv_msg(&mut t, "client recv").await;
                assert_eq!(got, data, "conn {i} msg {m} payload mismatch");
            }
        }));
    }

    // Busy-poll server: accept CONNS on the pool (round-robin across cores) and
    // echo MSGS on each connection's owning core.
    let servers = pool
        .serve(listener.clone(), config.clone(), CONNS, move |t| {
            server_echo_loop(t, MSGS)
        })
        .await;

    for (i, h) in clients.into_iter().enumerate() {
        h.await
            .unwrap_or_else(|e| panic!("client {i} panicked: {e}"));
    }
    for h in servers {
        h.await.expect("server conn task panicked");
    }

    pool.shutdown();
    drop(probe);
}

// Slice D3: a `with_config` pool sizes its shared CQs from the config and caps
// admission per core; a connect past the cap is refused, and a freed slot
// re-opens headroom.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_ring_busy_pool_admission_cap() {
    require_no_iwarp!();

    const CAP: usize = 2;
    const MSG_LEN: usize = 256;

    let Some((_serial, core_ids)) = busy_pool_guard(1).await else {
        return;
    };

    let config = ReadRingConfig::default();
    let listener = Arc::new(bind_listener_with_retry().await);
    let connect_addr = connect_addr_for(listener.local_addr());

    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    // One core, cap CAP: shared CQs sized from the config (D3), admission capped.
    let pool = BusyPool::with_config(ctx, &core_ids, &config, CAP).expect("build capped pool");

    // Arm-park server accepts exactly the CAP admitted connections, echoing one.
    let server_cfg = config.clone();
    let server = tokio::spawn(async move {
        let mut conns = Vec::with_capacity(CAP);
        for _ in 0..CAP {
            let t = ReadRingTransport::accept(&listener, server_cfg.clone())
                .await
                .expect("server accept");
            conns.push(tokio::spawn(server_echo_loop(t, 1)));
        }
        for h in conns {
            h.await.expect("server echo task panicked");
        }
    });

    // A gate that keeps the admitted connections alive (holding their slots)
    // until we have asserted the cap is enforced.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));

    let mut held = Vec::with_capacity(CAP);
    for i in 0..CAP {
        let cfg = config.clone();
        let gate = gate.clone();
        held.push(
            pool.spawn_connect(connect_addr, cfg, move |mut t| async move {
                let data = payload(i, MSG_LEN);
                send_msg(&mut t, &data, "held client send").await;
                let got = recv_msg(&mut t, "held client recv").await;
                assert_eq!(got, data, "held conn {i} payload mismatch");
                // Hold the admission slot until released.
                let _ = gate.acquire().await;
            })
            .expect("within cap admits"),
        );
    }

    // `spawn_connect` reserves the slot synchronously, so the cap is already
    // reached: the next connect is refused (no await between, so no held task
    // could have freed a slot yet).
    assert_eq!(pool.admitted_counts(), vec![CAP], "both slots admitted");
    assert!(
        pool.spawn_connect(connect_addr, config.clone(), |_t| async {})
            .is_none(),
        "connect past the per-core cap must be refused"
    );

    // Release the held connections; their guards free the admission slots.
    gate.add_permits(CAP);
    for (i, h) in held.into_iter().enumerate() {
        h.await
            .unwrap_or_else(|e| panic!("held {i} panicked: {e}"))
            .unwrap_or_else(|e| panic!("held {i} connect failed: {e}"));
    }
    server.await.expect("server task panicked");

    // Every slot is freed again once the connections have been reclaimed.
    assert_eq!(pool.admitted_counts(), vec![0], "slots freed after close");

    pool.shutdown();
    drop(probe);
}

// Slice D correctness: many connections on ONE core share a single busy-polled
// CQ, under a hot/idle load mix (some connections stream heavily, others barely).
// The per-core driver must reap the shared CQ and route each completion to the
// right connection's inbox with no starvation and no cross-talk — every
// connection completes its own sequence and every echo matches its own payload.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_ring_busy_pool_shared_cq_fairness() {
    require_no_iwarp!();

    const CONNS: usize = 6; // all on ONE core => one shared CQ
    const HOT_MSGS: usize = 300; // heavy streamers (even ids)
    const COOL_MSGS: usize = 5; // light streamers (odd ids)
    const MSG_LEN: usize = 256;

    let Some((_serial, core_ids)) = busy_pool_guard(1).await else {
        return;
    };

    let config = ReadRingConfig::default();
    let listener = bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    // Arm-park server: accept CONNS, echo each until its client disconnects
    // (clients send different, unknown message counts).
    let server_cfg = config.clone();
    let server = tokio::spawn(async move {
        let mut conns = Vec::with_capacity(CONNS);
        for _ in 0..CONNS {
            let t = ReadRingTransport::accept(&listener, server_cfg.clone())
                .await
                .expect("server accept");
            conns.push(tokio::spawn(server_echo_until_close(t)));
        }
        for h in conns {
            h.await.expect("server echo task panicked");
        }
    });

    // Single-core pool: all CONNS share ONE busy-polled CQ.
    let pool = BusyPool::new(ctx, &core_ids, 1024, 1024).expect("build busy pool");

    let mut clients = Vec::with_capacity(CONNS);
    for i in 0..CONNS {
        let cfg = config.clone();
        let msgs = if i % 2 == 0 { HOT_MSGS } else { COOL_MSGS };
        clients.push(
            pool.spawn_connect(connect_addr, cfg, move |mut t| async move {
                for m in 0..msgs {
                    // Seed the payload by connection id so a mis-routed echo
                    // (another connection's data) is caught.
                    let data = payload(i * 1_000_000 + m, MSG_LEN);
                    send_msg(&mut t, &data, "client send").await;
                    let got = recv_msg(&mut t, "client recv").await;
                    assert_eq!(got, data, "conn {i} msg {m} echo mismatch (cross-talk?)");
                }
            })
            .expect("uncapped pool always admits"),
        );
    }

    for (i, h) in clients.into_iter().enumerate() {
        h.await
            .unwrap_or_else(|e| panic!("client {i} task panicked: {e}"))
            .unwrap_or_else(|e| panic!("client {i} connect_busy failed: {e}"));
    }
    server.await.expect("server task panicked");

    pool.shutdown();
    drop(probe);
}

// Slice D correctness: one connection on a shared core disconnects mid-stream
// (its transport `Drop` triggers the busy reclaim barrier on that core) while
// its siblings on the SAME core keep streaming. The reclaim of the departing
// connection must not disrupt the survivors — they complete their full sequence
// with correct echoes, and the pool shuts down cleanly (reclaim drained).
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn read_ring_busy_pool_disconnect_isolation() {
    require_no_iwarp!();

    const CONNS: usize = 4; // all on one core
    const EARLY_MSGS: usize = 3; // conn 0 drops early
    const SURVIVOR_MSGS: usize = 150; // the rest keep streaming
    const MSG_LEN: usize = 256;

    let Some((_serial, core_ids)) = busy_pool_guard(1).await else {
        return;
    };

    let config = ReadRingConfig::default();
    let listener = bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let probe = AsyncCmId::new(PortSpace::Tcp).unwrap();
    probe.resolve_addr(None, &connect_addr, 2000).await.unwrap();
    let ctx = probe.verbs_context().expect("probe has no verbs context");

    let server_cfg = config.clone();
    let server = tokio::spawn(async move {
        let mut conns = Vec::with_capacity(CONNS);
        for _ in 0..CONNS {
            let t = ReadRingTransport::accept(&listener, server_cfg.clone())
                .await
                .expect("server accept");
            conns.push(tokio::spawn(server_echo_until_close(t)));
        }
        for h in conns {
            h.await.expect("server echo task panicked");
        }
    });

    let pool = BusyPool::new(ctx, &core_ids, 1024, 1024).expect("build busy pool");

    let mut clients = Vec::with_capacity(CONNS);
    for i in 0..CONNS {
        let cfg = config.clone();
        // conn 0 drops after a few messages (early disconnect + reclaim);
        // conns 1..CONNS keep streaming a long sequence on the same core.
        let msgs = if i == 0 { EARLY_MSGS } else { SURVIVOR_MSGS };
        clients.push(
            pool.spawn_connect(connect_addr, cfg, move |mut t| async move {
                for m in 0..msgs {
                    let data = payload(i * 1_000_000 + m, MSG_LEN);
                    send_msg(&mut t, &data, "client send").await;
                    let got = recv_msg(&mut t, "client recv").await;
                    assert_eq!(got, data, "conn {i} msg {m} echo mismatch");
                }
                // Returning here drops the transport: conn 0 tears down (busy
                // reclaim on the shared core) while the survivors are still
                // streaming through the same driver.
            })
            .expect("uncapped pool always admits"),
        );
    }

    for (i, h) in clients.into_iter().enumerate() {
        h.await
            .unwrap_or_else(|e| panic!("client {i} task panicked: {e}"))
            .unwrap_or_else(|e| panic!("client {i} connect_busy failed: {e}"));
    }
    server.await.expect("server task panicked");

    pool.shutdown();
    drop(probe);
}
