//! Async CQ integration tests — Phase A: CompletionChannel + AsyncCq.
//!
//! Tests verify that async CQ notification works with siw over the
//! tokio runtime. Connection setup is done synchronously on std::threads,
//! then the CompletionChannel + CQ are handed to the async test for
//! AsyncCq polling.

use std::thread;

use rdma_io::async_cq::AsyncCq;
use rdma_io::cm::{CmEventType, CmId, ConnParam, EventChannel, PortSpace};
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

/// Server-side resources returned after synchronous setup.
struct ServerSetup {
    _server_id: CmId,
    _server_ch: EventChannel,
    comp_ch: CompletionChannel,
    cq: std::sync::Arc<CompletionQueue>,
    recv_mr: OwnedMemoryRegion,
}

// Safety: RDMA resources are usable across threads.
unsafe impl Send for ServerSetup {}

/// Do all RDMA setup synchronously: server listens, client connects, client sends.
/// Returns the server's CompletionChannel + CQ for async polling.
fn setup_and_send(
    bind_addr: std::net::SocketAddr,
    connect_addr: std::net::SocketAddr,
    send_data: &[u8],
    recv_wr_id: u64,
) -> ServerSetup {
    let data_len = send_data.len();
    let send_buf = send_data.to_vec();

    let server_handle = thread::spawn(move || {
        let ch = EventChannel::new().unwrap();
        let listener = CmId::new(&ch, PortSpace::Tcp).unwrap();
        listener.listen(&bind_addr, 1).unwrap();

        let ev = ch.get_event().unwrap();
        assert_eq!(ev.event_type(), CmEventType::ConnectRequest);
        let server_id = unsafe { CmId::from_raw(ev.cm_id_raw(), true) };
        ev.ack();

        let pd = server_id.alloc_pd().unwrap();
        let ctx = server_id.verbs_context().unwrap();
        let comp_ch = CompletionChannel::new(&ctx).unwrap();
        let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();

        server_id
            .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
            .unwrap();

        let recv_mr = pd
            .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
            .unwrap();

        let server_qp = server_id.qp_raw();
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

        server_id.accept(&ConnParam::default()).unwrap();
        let ev = ch.get_event().unwrap();
        assert_eq!(ev.event_type(), CmEventType::Established);
        ev.ack();

        // Wait for disconnect from client
        let ev = ch.get_event().unwrap();
        assert_eq!(ev.event_type(), CmEventType::Disconnected);
        ev.ack();

        ServerSetup {
            _server_id: server_id,
            _server_ch: ch,
            comp_ch,
            cq,
            recv_mr,
        }
    });

    // Client: connect, send, disconnect
    thread::sleep(std::time::Duration::from_millis(50));

    let ch = EventChannel::new().unwrap();
    let client_id = CmId::new(&ch, PortSpace::Tcp).unwrap();

    client_id.resolve_addr(None, &connect_addr, 2000).unwrap();
    ch.get_event().unwrap().ack();
    client_id.resolve_route(2000).unwrap();
    ch.get_event().unwrap().ack();

    let pd = client_id.alloc_pd().unwrap();
    client_id.create_qp(&pd, &default_qp_attr()).unwrap();
    client_id.connect(&ConnParam::default()).unwrap();
    let ev = ch.get_event().unwrap();
    assert_eq!(ev.event_type(), CmEventType::Established);
    ev.ack();

    let mut padded = vec![0u8; 64];
    padded[..data_len].copy_from_slice(&send_buf);
    let send_mr = pd.reg_mr_owned(padded, AccessFlags::LOCAL_WRITE).unwrap();

    let client_qp = client_id.qp_raw();
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

    // Disconnect client
    client_id.disconnect().unwrap();
    let _ev = ch.get_event().unwrap();

    server_handle.join().expect("server thread panicked")
}

/// Test: AsyncCq::poll() receives a completion via comp_channel notification.
#[tokio::test]
async fn async_cq_send_recv() {
    let (bind_addr, connect_addr) = test_addrs();
    let setup = setup_and_send(bind_addr, connect_addr, b"async hello!", 100);

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
#[tokio::test]
async fn async_cq_poll_wr_id() {
    let (bind_addr, connect_addr) = test_addrs();
    let setup = setup_and_send(bind_addr, connect_addr, b"wr_id", 42);

    let notifier = TokioCqNotifier::new(setup.comp_ch.fd()).unwrap();
    let async_cq = AsyncCq::new(setup.cq, setup.comp_ch, Box::new(notifier));

    let wc = async_cq.poll_wr_id(42).await.unwrap();
    assert!(wc.is_success());
    assert_eq!(wc.byte_len(), 5);
    assert_eq!(&setup.recv_mr.as_slice()[..5], b"wr_id");
    println!("async_cq_poll_wr_id test passed!");
}
