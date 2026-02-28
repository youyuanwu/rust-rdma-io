//! rdma_cm integration tests — loopback connect, data transfer, disconnect.
//!
//! siw on loopback (127.0.0.1) doesn't support rdma_listen, so these tests
//! bind to 0.0.0.0 and connect via the eth0 IP using the siw0 device.

use std::thread;

use rdma_io::cm::{CmEventType, CmId, ConnParam, EventChannel, PortSpace};
use rdma_io::mr::AccessFlags;
use rdma_io::qp::QpInitAttr;
use rdma_io::wc::WorkCompletion;
use rdma_io::wr::QpType;

/// Pick a unique port per test, with the server binding to 0.0.0.0
/// and the client connecting to the first non-loopback IPv4 address.
fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    use std::sync::atomic::{AtomicU16, Ordering};
    static PORT: AtomicU16 = AtomicU16::new(39876);
    let port = PORT.fetch_add(1, Ordering::Relaxed);

    let bind_addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let connect_addr: std::net::SocketAddr = format!("{}:{port}", local_ip()).parse().unwrap();
    (bind_addr, connect_addr)
}

/// Discover the first non-loopback IPv4 address (for siw0 over eth0).
fn local_ip() -> String {
    use std::net::UdpSocket;
    // Trick: connect a UDP socket to an external address to find the local IP.
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

#[test]
fn cm_loopback_connect_disconnect() {
    let (bind_addr, connect_addr) = test_addrs();

    // --- Server thread ---
    let server = thread::spawn(move || {
        let ch = EventChannel::new().expect("server EventChannel");
        let listener = CmId::new(&ch, PortSpace::Tcp).expect("server CmId");
        listener.listen(&bind_addr, 1).expect("listen");

        // Wait for connect request
        let ev = ch.get_event().expect("server CONNECT_REQUEST");
        assert_eq!(ev.event_type(), CmEventType::ConnectRequest);
        let server_id = unsafe { CmId::from_raw(ev.cm_id_raw(), true) };
        ev.ack();

        let pd = server_id.alloc_pd().expect("server alloc_pd");
        server_id
            .create_qp(&pd, &default_qp_attr())
            .expect("server create_qp");

        server_id.accept(&ConnParam::default()).expect("accept");

        let ev = ch.get_event().expect("server ESTABLISHED");
        assert_eq!(ev.event_type(), CmEventType::Established);
        ev.ack();

        println!("Server: connected, qp_num={}", server_id.qp_num().unwrap());

        // Wait for disconnect
        let ev = ch.get_event().expect("server DISCONNECTED");
        assert_eq!(ev.event_type(), CmEventType::Disconnected);
        ev.ack();
        println!("Server: disconnected");
    });

    // --- Client (main thread) ---
    // Small delay to let server bind+listen
    thread::sleep(std::time::Duration::from_millis(50));

    let ch = EventChannel::new().expect("client EventChannel");
    let client_id = CmId::new(&ch, PortSpace::Tcp).expect("client CmId");

    client_id
        .resolve_addr(None, &connect_addr, 2000)
        .expect("resolve_addr");
    let ev = ch.get_event().expect("client ADDR_RESOLVED");
    assert_eq!(ev.event_type(), CmEventType::AddrResolved);
    ev.ack();

    client_id.resolve_route(2000).expect("resolve_route");
    let ev = ch.get_event().expect("client ROUTE_RESOLVED");
    assert_eq!(ev.event_type(), CmEventType::RouteResolved);
    ev.ack();

    let pd = client_id.alloc_pd().expect("client alloc_pd");
    client_id
        .create_qp(&pd, &default_qp_attr())
        .expect("client create_qp");

    client_id.connect(&ConnParam::default()).expect("connect");

    let ev = ch.get_event().expect("client ESTABLISHED");
    assert_eq!(ev.event_type(), CmEventType::Established);
    ev.ack();

    println!("Client: connected, qp_num={}", client_id.qp_num().unwrap());

    client_id.disconnect().expect("client disconnect");

    let ev = ch.get_event().expect("client DISCONNECTED");
    assert_eq!(ev.event_type(), CmEventType::Disconnected);
    ev.ack();

    server.join().expect("server thread panicked");
    println!("Connect/disconnect test passed!");
}

#[test]
fn cm_loopback_send_recv() {
    let (bind_addr, connect_addr) = test_addrs();

    // --- Server thread ---
    let server = thread::spawn(move || {
        let ch = EventChannel::new().unwrap();
        let listener = CmId::new(&ch, PortSpace::Tcp).unwrap();
        listener.listen(&bind_addr, 1).unwrap();

        let ev = ch.get_event().unwrap();
        assert_eq!(ev.event_type(), CmEventType::ConnectRequest);
        let server_id = unsafe { CmId::from_raw(ev.cm_id_raw(), true) };
        ev.ack();

        let pd = server_id.alloc_pd().unwrap();
        server_id.create_qp(&pd, &default_qp_attr()).unwrap();

        // Register recv buffer and post recv BEFORE accept
        let recv_mr = pd
            .reg_mr_owned(vec![0u8; 64], AccessFlags::LOCAL_WRITE)
            .unwrap();

        let server_qp = server_id.qp_raw();
        let server_recv_cq = unsafe { (*server_qp).recv_cq };
        {
            let mut sge = rdma_io_sys::ibverbs::ibv_sge {
                addr: unsafe { (*recv_mr.as_raw()).addr as u64 },
                length: 64,
                lkey: recv_mr.lkey(),
            };
            let mut wr = rdma_io_sys::ibverbs::ibv_recv_wr {
                wr_id: 1,
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

        // Poll recv completion
        let mut wc = [WorkCompletion::default(); 1];
        loop {
            let n = unsafe {
                rdma_io_sys::wrapper::rdma_wrap_ibv_poll_cq(
                    server_recv_cq,
                    1,
                    wc.as_mut_ptr().cast(),
                )
            };
            if n > 0 {
                assert!(wc[0].is_success(), "recv WC error: {:?}", wc[0].status());
                assert_eq!(wc[0].byte_len(), 11);
                break;
            }
            assert!(n >= 0, "poll_cq error");
            std::hint::spin_loop();
        }

        assert_eq!(&recv_mr.as_slice()[..11], b"hello rdma!");
        println!(
            "Server received: {}",
            std::str::from_utf8(&recv_mr.as_slice()[..11]).unwrap()
        );

        // Wait for disconnect
        let ev = ch.get_event().unwrap();
        assert_eq!(ev.event_type(), CmEventType::Disconnected);
        ev.ack();
    });

    // --- Client (main thread) ---
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

    // --- Send data ---
    let mut send_data = vec![0u8; 64];
    send_data[..11].copy_from_slice(b"hello rdma!");
    let send_mr = pd
        .reg_mr_owned(send_data, AccessFlags::LOCAL_WRITE)
        .unwrap();

    let client_qp = client_id.qp_raw();
    let client_send_cq = unsafe { (*client_qp).send_cq };
    {
        let mut sge = rdma_io_sys::ibverbs::ibv_sge {
            addr: unsafe { (*send_mr.as_raw()).addr as u64 },
            length: 11,
            lkey: send_mr.lkey(),
        };
        let mut wr = rdma_io_sys::ibverbs::ibv_send_wr {
            wr_id: 2,
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

    // Poll send completion
    let mut wc = [WorkCompletion::default(); 1];
    loop {
        let n = unsafe {
            rdma_io_sys::wrapper::rdma_wrap_ibv_poll_cq(client_send_cq, 1, wc.as_mut_ptr().cast())
        };
        if n > 0 {
            assert!(wc[0].is_success(), "send WC error: {:?}", wc[0].status());
            break;
        }
        assert!(n >= 0, "poll_cq error");
        std::hint::spin_loop();
    }
    println!("Client send completed");

    // Cleanup
    drop(send_mr);
    client_id.disconnect().unwrap();
    let ev = ch.get_event().unwrap();
    assert_eq!(ev.event_type(), CmEventType::Disconnected);
    ev.ack();

    server.join().expect("server thread panicked");
    println!("Send/recv test passed!");
}
