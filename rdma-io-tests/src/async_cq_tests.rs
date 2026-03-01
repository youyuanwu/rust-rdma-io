//! Async CQ integration tests — Phase A: CompletionChannel + AsyncCq.
//!
//! Tests verify that async CQ notification works with siw over the
//! tokio runtime. Connection setup uses async CM (AsyncCmId/AsyncCmListener),
//! but raw ibv_post_recv/ibv_post_send + spin-poll for the data path to
//! isolate AsyncCq testing from AsyncQp.

use rdma_io::async_cm::{AsyncCmId, AsyncCmListener};
use rdma_io::async_cq::AsyncCq;
use rdma_io::cm::{CmEventType, ConnParam, PortSpace};
use rdma_io::comp_channel::CompletionChannel;
use rdma_io::cq::CompletionQueue;
use rdma_io::mr::{AccessFlags, OwnedMemoryRegion};
use rdma_io::qp::QpInitAttr;
use rdma_io::tokio_notifier::TokioCqNotifier;
use rdma_io::wc::WorkCompletion;
use rdma_io::wr::QpType;

fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    use std::sync::atomic::{AtomicU16, Ordering};
    static PORT: AtomicU16 = AtomicU16::new(40100);
    let port = PORT.fetch_add(1, Ordering::Relaxed);
    let bind_addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let connect_addr: std::net::SocketAddr = format!("{}:{port}", local_ip()).parse().unwrap();
    (bind_addr, connect_addr)
}

fn local_ip() -> String {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
    sock.connect("8.8.8.8:80").unwrap();
    sock.local_addr().unwrap().ip().to_string()
}

fn default_qp_attr() -> QpInitAttr {
    QpInitAttr {
        qp_type: QpType::Rc,
        max_send_wr: 16,
        max_recv_wr: 16,
        max_send_sge: 1,
        max_recv_sge: 1,
        ..Default::default()
    }
}

/// Server-side resources returned after setup.
struct ServerSetup {
    _server_cm: AsyncCmId,
    comp_ch: CompletionChannel,
    cq: std::sync::Arc<CompletionQueue>,
    recv_mr: OwnedMemoryRegion,
}

// Safety: RDMA resources are usable across threads.
unsafe impl Send for ServerSetup {}

/// Set up connection via async CM, send data via raw verbs, return server CQ.
///
/// Uses AsyncCmId/AsyncCmListener for connection setup and disconnect,
/// but raw ibv_post_recv/ibv_post_send + spin-poll for the data path
/// to isolate AsyncCq testing.
async fn setup_and_send(
    bind_addr: std::net::SocketAddr,
    connect_addr: std::net::SocketAddr,
    send_data: &[u8],
    recv_wr_id: u64,
) -> ServerSetup {
    let data_len = send_data.len();
    let send_buf = send_data.to_vec();

    let listener = AsyncCmListener::bind(&bind_addr).unwrap();

    let server_handle = tokio::spawn(async move {
        // Two-phase accept: get request, set up QP + recv, then complete
        let conn_id = listener.get_request().await.unwrap();

        let pd = conn_id.alloc_pd().unwrap();
        let ctx = conn_id.verbs_context().unwrap();
        let comp_ch = CompletionChannel::new(&ctx).unwrap();
        let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();

        conn_id
            .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
            .unwrap();

        let recv_mr = pd
            .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
            .unwrap();

        // Post raw recv before accept (tests raw verb path)
        let server_qp = conn_id.qp_raw();
        {
            let mut sge = rdma_io_sys::ibverbs::ibv_sge {
                addr: unsafe { (*recv_mr.as_raw()).addr as u64 },
                length: 64,
                lkey: recv_mr.lkey(),
            };
            let mut wr = rdma_io_sys::ibverbs::ibv_recv_wr {
                wr_id: recv_wr_id,
                sg_list: &mut sge,
                num_sge: 1,
                ..Default::default()
            };
            let mut bad_wr: *mut rdma_io_sys::ibverbs::ibv_recv_wr = std::ptr::null_mut();
            let ret = unsafe {
                rdma_io_sys::wrapper::rdma_wrap_ibv_post_recv(server_qp, &mut wr, &mut bad_wr)
            };
            assert_eq!(ret, 0, "server post_recv failed");
        }

        let server_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();

        // Await disconnect from client
        let ev = server_cm.next_event().await.unwrap();
        assert_eq!(ev.event_type(), CmEventType::Disconnected);
        ev.ack();

        ServerSetup {
            _server_cm: server_cm,
            comp_ch,
            cq,
            recv_mr,
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Client: async connect, raw send, async disconnect
    let client_cm = AsyncCmId::new(PortSpace::Tcp).unwrap();
    client_cm
        .resolve_addr(None, &connect_addr, 2000)
        .await
        .unwrap();
    client_cm.resolve_route(2000).await.unwrap();

    let pd = client_cm.alloc_pd().unwrap();
    // No CompletionChannel — client uses spin-poll (testing CQ layer, not QP layer)
    client_cm
        .cm_id()
        .create_qp(&pd, &default_qp_attr())
        .unwrap();
    client_cm.connect(&ConnParam::default()).await.unwrap();

    // Raw post_send + spin-poll
    let mut padded = vec![0u8; 64];
    padded[..data_len].copy_from_slice(&send_buf);
    let send_mr = pd.reg_mr_owned(padded, AccessFlags::LOCAL_WRITE).unwrap();

    let client_qp = client_cm.qp_raw();
    let client_send_cq = unsafe { (*client_qp).send_cq };
    {
        let mut sge = rdma_io_sys::ibverbs::ibv_sge {
            addr: unsafe { (*send_mr.as_raw()).addr as u64 },
            length: data_len as u32,
            lkey: send_mr.lkey(),
        };
        let mut wr = rdma_io_sys::ibverbs::ibv_send_wr {
            wr_id: 999,
            opcode: rdma_io_sys::ibverbs::IBV_WR_SEND,
            send_flags: rdma_io_sys::ibverbs::IBV_SEND_SIGNALED,
            sg_list: &mut sge,
            num_sge: 1,
            ..Default::default()
        };
        let mut bad_wr: *mut rdma_io_sys::ibverbs::ibv_send_wr = std::ptr::null_mut();
        let ret = unsafe {
            rdma_io_sys::wrapper::rdma_wrap_ibv_post_send(client_qp, &mut wr, &mut bad_wr)
        };
        assert_eq!(ret, 0, "client post_send failed");
    }

    // Spin-poll send completion
    let mut wc = [WorkCompletion::default(); 1];
    loop {
        let n = unsafe {
            rdma_io_sys::wrapper::rdma_wrap_ibv_poll_cq(client_send_cq, 1, wc.as_mut_ptr().cast())
        };
        if n > 0 {
            assert!(wc[0].is_success(), "send WC error: {:?}", wc[0].status());
            break;
        }
        assert!(n >= 0);
        std::hint::spin_loop();
    }

    // Async disconnect — await DISCONNECTED event
    client_cm.disconnect_async().await.unwrap();

    server_handle.await.expect("server task panicked")
}

/// Test: AsyncCq::poll() receives a completion via comp_channel notification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_cq_send_recv() {
    let (bind_addr, connect_addr) = test_addrs();
    let setup = setup_and_send(bind_addr, connect_addr, b"async hello!", 100).await;

    let notifier = TokioCqNotifier::new(setup.comp_ch.fd()).unwrap();
    let async_cq = AsyncCq::new(setup.cq, setup.comp_ch, Box::new(notifier));

    let mut wc = [WorkCompletion::default(); 4];
    let n = async_cq.poll(&mut wc).await.unwrap();
    assert!(n >= 1, "expected at least 1 completion, got {n}");

    let recv_wc = wc[..n].iter().find(|w| w.wr_id() == 100);
    assert!(recv_wc.is_some(), "recv completion (wr_id=100) not found");
    let recv_wc = recv_wc.unwrap();
    assert!(
        recv_wc.is_success(),
        "recv WC error: {:?}",
        recv_wc.status()
    );
    assert_eq!(recv_wc.byte_len(), 12);

    assert_eq!(&setup.recv_mr.as_slice()[..12], b"async hello!");
    println!("async_cq_send_recv test passed!");
}

/// Test: AsyncCq::poll_wr_id() waits for a specific WR ID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_cq_poll_wr_id() {
    let (bind_addr, connect_addr) = test_addrs();
    let setup = setup_and_send(bind_addr, connect_addr, b"wr_id", 42).await;

    let notifier = TokioCqNotifier::new(setup.comp_ch.fd()).unwrap();
    let async_cq = AsyncCq::new(setup.cq, setup.comp_ch, Box::new(notifier));

    let wc = async_cq.poll_wr_id(42).await.unwrap();
    assert!(wc.is_success());
    assert_eq!(wc.byte_len(), 5);
    assert_eq!(&setup.recv_mr.as_slice()[..5], b"wr_id");
    println!("async_cq_poll_wr_id test passed!");
}
