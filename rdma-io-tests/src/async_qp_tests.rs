//! AsyncQp integration tests — Phases B + E.
//!
//! Phase B: send/recv via AsyncQp.
//! Phase E: one-sided RDMA READ/WRITE + atomic verbs.
//!
//! Tests verify that `AsyncQp` correctly posts verbs and awaits completions.
//! Connection setup uses async CM (AsyncCmId/AsyncCmListener) for
//! non-blocking connect/accept, then AsyncQp is constructed for verb operations.

use std::sync::Arc;

use rdma_io::async_cm::{AsyncCmId, AsyncCmListener};
use rdma_io::async_cq::AsyncCq;
use rdma_io::async_qp::AsyncQp;
use rdma_io::cm::{ConnParam, PortSpace};
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

/// Holds resources that must stay alive while AsyncQp is in use.
struct AsyncEndpoint {
    aqp: AsyncQp,
    pd: Arc<ProtectionDomain>,
    _cm: AsyncCmId,
}

/// Set up a connected server+client pair using async CM.
/// Returns (server_endpoint, client_endpoint) ready for async verb posting.
async fn setup_connection(
    bind_addr: std::net::SocketAddr,
    connect_addr: std::net::SocketAddr,
) -> (AsyncEndpoint, AsyncEndpoint) {
    let listener = AsyncCmListener::bind(&bind_addr).unwrap();

    let server_handle = tokio::spawn(async move {
        // Two-phase accept: get request, set up QP, then complete
        let conn_id = listener.get_request().await.unwrap();

        let pd = conn_id.alloc_pd().unwrap();
        let ctx = conn_id.verbs_context().unwrap();
        let comp_ch = CompletionChannel::new(&ctx).unwrap();
        let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();

        conn_id
            .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
            .unwrap();

        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();

        let qp_raw = async_cm.qp_raw();
        let notifier = TokioCqNotifier::new(comp_ch.fd()).unwrap();
        let async_cq = AsyncCq::new(cq, comp_ch, Box::new(notifier));
        let aqp = unsafe { AsyncQp::new(qp_raw, async_cq) };

        AsyncEndpoint {
            aqp,
            pd,
            _cm: async_cm,
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle = tokio::spawn(async move {
        // Step-by-step connect: resolve → QP setup → connect
        let async_cm = AsyncCmId::new(PortSpace::Tcp).unwrap();
        async_cm
            .resolve_addr(None, &connect_addr, 2000)
            .await
            .unwrap();
        async_cm.resolve_route(2000).await.unwrap();

        let pd = async_cm.alloc_pd().unwrap();
        let ctx = async_cm.verbs_context().unwrap();
        let comp_ch = CompletionChannel::new(&ctx).unwrap();
        let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();

        async_cm
            .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
            .unwrap();

        async_cm.connect(&ConnParam::default()).await.unwrap();

        let qp_raw = async_cm.qp_raw();
        let notifier = TokioCqNotifier::new(comp_ch.fd()).unwrap();
        let async_cq = AsyncCq::new(cq, comp_ch, Box::new(notifier));
        let aqp = unsafe { AsyncQp::new(qp_raw, async_cq) };

        AsyncEndpoint {
            aqp,
            pd,
            _cm: async_cm,
        }
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    (server_res.unwrap(), client_res.unwrap())
}

/// Test: AsyncQp send/recv roundtrip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_qp_send_recv() {
    let (bind_addr, connect_addr) = test_addrs();
    let (server, client) = setup_connection(bind_addr, connect_addr).await;

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
    let (server, client) = setup_connection(bind_addr, connect_addr).await;

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
    let (server, client) = setup_connection(bind_addr, connect_addr).await;

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

// --- Async CM lifecycle tests ---

/// Test: async disconnect — client disconnects, server detects via next_event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_cm_disconnect() {
    let (bind_addr, connect_addr) = test_addrs();

    let listener = AsyncCmListener::bind(&bind_addr).unwrap();

    // Server: two-phase accept, keep AsyncCmId alive for event monitoring
    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();

        let pd = conn_id.alloc_pd().unwrap();
        let ctx = conn_id.verbs_context().unwrap();
        let comp_ch = CompletionChannel::new(&ctx).unwrap();
        let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();
        conn_id
            .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
            .unwrap();

        let server_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();

        // Wait for disconnect event
        let event = server_cm.next_event().await.unwrap();
        let etype = event.event_type();
        event.ack();
        assert_eq!(
            etype,
            rdma_io::cm::CmEventType::Disconnected,
            "expected Disconnected, got {etype:?}"
        );
        println!("  server: received Disconnected event");
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Client: connect then async disconnect
    let client_handle = tokio::spawn(async move {
        let client_cm = AsyncCmId::new(PortSpace::Tcp).unwrap();
        client_cm
            .resolve_addr(None, &connect_addr, 2000)
            .await
            .unwrap();
        client_cm.resolve_route(2000).await.unwrap();

        let pd = client_cm.alloc_pd().unwrap();
        let ctx = client_cm.verbs_context().unwrap();
        let comp_ch = CompletionChannel::new(&ctx).unwrap();
        let cq = CompletionQueue::with_comp_channel(ctx, 16, &comp_ch).unwrap();
        client_cm
            .create_qp_with_cq(&pd, &default_qp_attr(), Some(&cq), Some(&cq))
            .unwrap();

        client_cm.connect(&ConnParam::default()).await.unwrap();
        println!("  client: connected, disconnecting...");

        // Graceful async disconnect — await DISCONNECTED event
        client_cm.disconnect_async().await.unwrap();
        println!("  client: async disconnect complete");
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    server_res.unwrap();
    client_res.unwrap();

    println!("async_cm_disconnect passed!");
}
