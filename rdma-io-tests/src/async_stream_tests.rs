//! AsyncRdmaStream integration tests — Phase C.
//!
//! Tests verify that `AsyncRdmaStream` provides TCP-like async read/write
//! over RDMA SEND/RECV, using completion-channel-driven async CQ polling.
//!
//! Note: `AsyncRdmaListener::accept()` and `AsyncRdmaStream::connect()` create
//! `TokioCqNotifier` internally, which requires a tokio reactor context.
//! We use `tokio::task::spawn_blocking` (not `std::thread::spawn`) so the
//! tokio Handle is available on the blocking thread.

use rdma_io::async_stream::{AsyncRdmaListener, AsyncRdmaStream};

fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    use std::sync::atomic::{AtomicU16, Ordering};
    static PORT: AtomicU16 = AtomicU16::new(40300);
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

/// Test: async echo — client sends, server reads and echoes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn async_stream_echo() {
    let (bind_addr, connect_addr) = test_addrs();

    let server_handle = tokio::task::spawn_blocking(move || {
        let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
        listener.accept().unwrap()
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle =
        tokio::task::spawn_blocking(move || AsyncRdmaStream::connect(&connect_addr).unwrap());

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let mut server = server_res.unwrap();
    let mut client = client_res.unwrap();

    // Client sends, server reads
    let send_data = b"async stream hello!";
    let n = client.write(send_data).await.unwrap();
    assert_eq!(n, send_data.len());

    let mut buf = [0u8; 1024];
    let n = server.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], send_data);

    // Server echoes back
    let n = server.write(&buf[..n]).await.unwrap();
    assert_eq!(n, send_data.len());

    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], send_data);

    println!("async_stream_echo passed!");
}

/// Test: multi-message — 5 round-trips of ping/pong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_stream_multi_message() {
    let (bind_addr, connect_addr) = test_addrs();

    let server_handle = tokio::task::spawn_blocking(move || {
        let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
        listener.accept().unwrap()
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client =
        tokio::task::spawn_blocking(move || AsyncRdmaStream::connect(&connect_addr).unwrap())
            .await
            .unwrap();

    let mut server = server_handle.await.unwrap();

    for i in 0..5u32 {
        let msg = format!("message-{i}");
        client.write(msg.as_bytes()).await.unwrap();

        let mut buf = [0u8; 256];
        let n = server.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], msg.as_bytes(), "round {i} server read mismatch");

        let reply = format!("reply-{i}");
        server.write(reply.as_bytes()).await.unwrap();

        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            reply.as_bytes(),
            "round {i} client read mismatch"
        );
    }

    println!("async_stream_multi_message passed!");
}

/// Test: large transfer — send 32 KiB in one write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_stream_large_transfer() {
    let (bind_addr, connect_addr) = test_addrs();

    let server_handle = tokio::task::spawn_blocking(move || {
        let listener = AsyncRdmaListener::bind(&bind_addr).unwrap();
        listener.accept().unwrap()
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client =
        tokio::task::spawn_blocking(move || AsyncRdmaStream::connect(&connect_addr).unwrap())
            .await
            .unwrap();

    let mut server = server_handle.await.unwrap();

    // 32 KiB of patterned data
    let data: Vec<u8> = (0..32768).map(|i| (i % 251) as u8).collect();
    let n = client.write(&data).await.unwrap();
    assert_eq!(n, data.len());

    // Server reads — may need multiple reads for partial delivery
    let mut received = Vec::new();
    let mut buf = [0u8; 65536];
    while received.len() < data.len() {
        let n = server.read(&mut buf).await.unwrap();
        assert!(n > 0, "unexpected EOF");
        received.extend_from_slice(&buf[..n]);
    }

    assert_eq!(received.len(), data.len());
    assert_eq!(received, data);

    println!("async_stream_large_transfer passed!");
}
