//! AsyncQp integration tests — Phases B + E.
//!
//! Phase B: send/recv via AsyncQp.
//! Phase E: one-sided RDMA READ/WRITE + atomic verbs.
//!
//! Tests verify that `AsyncQp` correctly posts verbs and awaits completions.
//! Connection setup uses rdma_cm synchronously on std::threads, then
//! AsyncQp is constructed for async verb operations.

use std::sync::Arc;
use std::thread;

use rdma_io::async_cq::AsyncCq;
use rdma_io::async_qp::AsyncQp;
use rdma_io::cm::{CmEventType, CmId, ConnParam, EventChannel, PortSpace};
use rdma_io::comp_channel::CompletionChannel;
use rdma_io::cq::CompletionQueue;
use rdma_io::mr::{AccessFlags, RemoteMr};
use rdma_io::pd::ProtectionDomain;
use rdma_io::qp::QpInitAttr;
use rdma_io::tokio_notifier::TokioCqNotifier;
use rdma_io::wr::QpType;

fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    use std::sync::atomic::{AtomicU16, Ordering};
    static PORT: AtomicU16 = AtomicU16::new(40200);
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

/// RDMA endpoint resources for one side of a connection.
struct Endpoint {
    _cm_id: CmId,
    _event_ch: EventChannel,
    pd: Arc<ProtectionDomain>,
    comp_ch: CompletionChannel,
    cq: Arc<CompletionQueue>,
    qp_raw: *mut rdma_io_sys::ibverbs::ibv_qp,
}

// Safety: RDMA resources are usable across threads.
unsafe impl Send for Endpoint {}

/// Set up a connected server+client pair synchronously.
/// Returns (server_endpoint, client_endpoint) ready for async verb posting.
fn setup_connection(
    bind_addr: std::net::SocketAddr,
    connect_addr: std::net::SocketAddr,
) -> (Endpoint, Endpoint) {
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

        let qp_raw = server_id.qp_raw();

        server_id.accept(&ConnParam::default()).unwrap();
        let ev = ch.get_event().unwrap();
        assert_eq!(ev.event_type(), CmEventType::Established);
        ev.ack();

        Endpoint {
            _cm_id: server_id,
            _event_ch: ch,
            pd,
            comp_ch,
            cq,
            qp_raw,
        }
    });

    // Small delay so server is listening
    thread::sleep(std::time::Duration::from_millis(50));

    let ch = EventChannel::new().unwrap();
    let client_id = CmId::new(&ch, PortSpace::Tcp).unwrap();

    client_id.resolve_addr(None, &connect_addr, 2000).unwrap();
    ch.get_event().unwrap().ack();
    client_id.resolve_route(2000).unwrap();
    ch.get_event().unwrap().ack();

    let pd = client_id.alloc_pd().unwrap();
    let ctx = client_id.verbs_context().unwrap();
    let comp_ch = CompletionChannel::new(&ctx).unwrap();
    let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();

    client_id
        .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
        .unwrap();

    let qp_raw = client_id.qp_raw();

    client_id.connect(&ConnParam::default()).unwrap();
    let ev = ch.get_event().unwrap();
    assert_eq!(ev.event_type(), CmEventType::Established);
    ev.ack();

    let server = server_handle.join().expect("server thread panicked");

    let client = Endpoint {
        _cm_id: client_id,
        _event_ch: ch,
        pd,
        comp_ch,
        cq,
        qp_raw,
    };

    (server, client)
}

/// Holds resources that must stay alive while AsyncQp is in use.
struct AsyncEndpoint {
    aqp: AsyncQp,
    pd: Arc<ProtectionDomain>,
    _cm_id: CmId,
    _event_ch: EventChannel,
}

fn make_async_endpoint(ep: Endpoint) -> AsyncEndpoint {
    let notifier = TokioCqNotifier::new(ep.comp_ch.fd()).unwrap();
    let async_cq = AsyncCq::new(ep.cq, ep.comp_ch, Box::new(notifier));
    let aqp = unsafe { AsyncQp::new(ep.qp_raw, async_cq) };
    AsyncEndpoint {
        aqp,
        pd: ep.pd,
        _cm_id: ep._cm_id,
        _event_ch: ep._event_ch,
    }
}

/// Test: AsyncQp send/recv roundtrip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_qp_send_recv() {
    let (bind_addr, connect_addr) = test_addrs();
    let (server_ep, client_ep) = setup_connection(bind_addr, connect_addr);

    let server = make_async_endpoint(server_ep);
    let client = make_async_endpoint(client_ep);

    let server_mr = server
        .pd
        .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
        .unwrap();
    let client_mr = client
        .pd
        .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
        .unwrap();

    // Write data into client MR
    unsafe {
        std::ptr::copy_nonoverlapping(
            b"async qp test".as_ptr(),
            (*client_mr.as_raw()).addr as *mut u8,
            13,
        );
    }

    // Server posts recv, client sends — run concurrently
    let server_fut = server.aqp.recv(&server_mr, 0, 64, 1);
    let client_fut = client.aqp.send(&client_mr, 0, 13, 2);

    let (recv_wc, send_wc) = tokio::join!(server_fut, client_fut);

    let recv_wc = recv_wc.unwrap();
    assert!(recv_wc.is_success(), "recv failed: {:?}", recv_wc.status());
    assert_eq!(recv_wc.byte_len(), 13);
    assert_eq!(&server_mr.as_slice()[..13], b"async qp test");

    let send_wc = send_wc.unwrap();
    assert!(send_wc.is_success(), "send failed: {:?}", send_wc.status());

    println!("async_qp_send_recv passed!");
}

/// Test: AsyncQp multi-message ping-pong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_qp_ping_pong() {
    let (bind_addr, connect_addr) = test_addrs();
    let (server_ep, client_ep) = setup_connection(bind_addr, connect_addr);

    let server = make_async_endpoint(server_ep);
    let client = make_async_endpoint(client_ep);

    let server_mr = server
        .pd
        .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
        .unwrap();
    let client_mr = client
        .pd
        .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
        .unwrap();

    // Ping: client → server
    unsafe {
        std::ptr::copy_nonoverlapping(b"ping".as_ptr(), (*client_mr.as_raw()).addr as *mut u8, 4);
    }

    let recv_fut = server.aqp.recv(&server_mr, 0, 64, 10);
    let send_fut = client.aqp.send(&client_mr, 0, 4, 11);
    let (recv_wc, send_wc) = tokio::join!(recv_fut, send_fut);
    assert!(recv_wc.unwrap().is_success());
    assert!(send_wc.unwrap().is_success());
    assert_eq!(&server_mr.as_slice()[..4], b"ping");

    // Pong: server → client
    unsafe {
        std::ptr::copy_nonoverlapping(b"pong".as_ptr(), (*server_mr.as_raw()).addr as *mut u8, 4);
    }

    let recv_fut = client.aqp.recv(&client_mr, 0, 64, 20);
    let send_fut = server.aqp.send(&server_mr, 0, 4, 21);
    let (recv_wc, send_wc) = tokio::join!(recv_fut, send_fut);
    assert!(recv_wc.unwrap().is_success());
    assert!(send_wc.unwrap().is_success());
    assert_eq!(&client_mr.as_slice()[..4], b"pong");

    println!("async_qp_ping_pong passed!");
}

// --- Phase E: One-sided RDMA READ/WRITE tests ---

/// Access flags for remote-accessible MRs.
const REMOTE_ACCESS: AccessFlags = AccessFlags::LOCAL_WRITE
    .union(AccessFlags::REMOTE_READ)
    .union(AccessFlags::REMOTE_WRITE);

/// Exchange remote MR info via SEND/RECV.
async fn exchange_remote_mr(
    sender: &AsyncQp,
    sender_mr: &rdma_io::mr::OwnedMemoryRegion,
    receiver: &AsyncQp,
    receiver_mr: &rdma_io::mr::OwnedMemoryRegion,
    remote: &RemoteMr,
) -> RemoteMr {
    // Serialize RemoteMr into sender_mr
    let bytes = remote.addr.to_le_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (*sender_mr.as_raw()).addr as *mut u8, 8);
        let rkey_bytes = remote.rkey.to_le_bytes();
        std::ptr::copy_nonoverlapping(
            rkey_bytes.as_ptr(),
            ((*sender_mr.as_raw()).addr as *mut u8).add(8),
            4,
        );
        let len_bytes = remote.len.to_le_bytes();
        std::ptr::copy_nonoverlapping(
            len_bytes.as_ptr(),
            ((*sender_mr.as_raw()).addr as *mut u8).add(12),
            4,
        );
    }

    let recv_fut = receiver.recv(receiver_mr, 0, 64, 50);
    let send_fut = sender.send(sender_mr, 0, 16, 51);
    let (recv_wc, send_wc) = tokio::join!(recv_fut, send_fut);
    assert!(recv_wc.unwrap().is_success());
    assert!(send_wc.unwrap().is_success());

    // Deserialize from receiver_mr
    let buf = receiver_mr.as_slice();
    let addr = u64::from_le_bytes(buf[..8].try_into().unwrap());
    let rkey = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let len = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    RemoteMr { addr, rkey, len }
}

/// Test: RDMA WRITE then RDMA READ roundtrip.
///
/// Client writes data to server's MR via RDMA WRITE (one-sided),
/// then reads it back via RDMA READ to verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_qp_rdma_write_read() {
    let (bind_addr, connect_addr) = test_addrs();
    let (server_ep, client_ep) = setup_connection(bind_addr, connect_addr);

    let server = make_async_endpoint(server_ep);
    let client = make_async_endpoint(client_ep);

    // Server MR with remote access
    let server_data_mr = server
        .pd
        .reg_mr_owned(vec![0u8; 256], REMOTE_ACCESS)
        .unwrap();
    let server_remote = server_data_mr.to_remote();

    // Messaging MRs for exchanging remote MR info
    let server_msg_mr = server
        .pd
        .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
        .unwrap();
    let client_msg_mr = client
        .pd
        .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
        .unwrap();

    // Client's local buffer for RDMA ops
    let mut client_buf_mr = client
        .pd
        .reg_mr_owned(vec![0u8; 256], AccessFlags::LOCAL_WRITE)
        .unwrap();

    // Exchange server's remote MR info → client
    let remote_info = exchange_remote_mr(
        &server.aqp,
        &server_msg_mr,
        &client.aqp,
        &client_msg_mr,
        &server_remote,
    )
    .await;

    // Client RDMA WRITE "hello rdma write" to server
    let write_data = b"hello rdma write";
    client_buf_mr.as_mut_slice()[..write_data.len()].copy_from_slice(write_data);

    let wc = client
        .aqp
        .write_remote(&client_buf_mr, 0, write_data.len(), &remote_info, 0, 100)
        .await
        .unwrap();
    assert!(wc.is_success(), "RDMA WRITE failed: {:?}", wc.status());

    // Verify server MR has the data
    assert_eq!(&server_data_mr.as_slice()[..write_data.len()], write_data);

    // Client RDMA READ back from server
    client_buf_mr.as_mut_slice()[..write_data.len()].fill(0); // clear local

    let wc = client
        .aqp
        .read_remote(&client_buf_mr, 0, write_data.len(), &remote_info, 0, 101)
        .await
        .unwrap();
    assert!(wc.is_success(), "RDMA READ failed: {:?}", wc.status());
    assert_eq!(&client_buf_mr.as_slice()[..write_data.len()], write_data);

    println!("async_qp_rdma_write_read passed!");
}

// Note: RDMA WRITE with Immediate and Atomic operations (CAS, FAA) are
// NOT supported by iWARP/siw. The API methods exist and are correct for
// InfiniBand/RoCE hardware. Tests for these will be added when hardware
// testing is available. See AsyncIntegration.md Phase E.
