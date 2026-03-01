//! Stream integration tests — RdmaListener/RdmaStream with std::io traits.

use std::io::{Read, Write};
use std::thread;

use rdma_io::stream::{RdmaListener, RdmaStream};

/// Discover the first non-loopback IPv4 address (siw0 over eth0).
fn local_ip() -> String {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
    sock.connect("8.8.8.8:80").unwrap();
    sock.local_addr().unwrap().ip().to_string()
}

/// Pick unique bind/connect addresses per test.
fn test_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    use std::sync::atomic::{AtomicU16, Ordering};
    static PORT: AtomicU16 = AtomicU16::new(49876);
    let port = PORT.fetch_add(1, Ordering::Relaxed);
    let bind: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let connect: std::net::SocketAddr = format!("{}:{port}", local_ip()).parse().unwrap();
    (bind, connect)
}

#[test]
fn stream_echo() {
    let (bind_addr, connect_addr) = test_addrs();

    let server = thread::spawn(move || {
        let listener = RdmaListener::bind(&bind_addr).expect("bind");
        let mut stream = listener.accept().expect("accept");
        println!("Server: accepted connection");

        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).expect("read");
        println!("Server: received {} bytes", n);

        // Echo back
        stream.write_all(&buf[..n]).expect("write");
        println!("Server: echoed {} bytes", n);
    });

    thread::sleep(std::time::Duration::from_millis(100));

    let mut client = RdmaStream::connect(&connect_addr).expect("connect");
    println!("Client: connected");

    let msg = b"hello rdma stream!";
    client.write_all(msg).expect("write");
    println!("Client: sent {} bytes", msg.len());

    let mut buf = [0u8; 1024];
    let n = client.read(&mut buf).expect("read");
    assert_eq!(&buf[..n], msg);
    println!(
        "Client: received echo: {}",
        std::str::from_utf8(&buf[..n]).unwrap()
    );

    server.join().expect("server panicked");
    println!("Stream echo test passed!");
}

#[test]
fn stream_multi_message() {
    let (bind_addr, connect_addr) = test_addrs();
    let num_messages = 5;

    let server = thread::spawn(move || {
        let listener = RdmaListener::bind(&bind_addr).expect("bind");
        let mut stream = listener.accept().expect("accept");

        for i in 0..num_messages {
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).expect("read");
            let msg = std::str::from_utf8(&buf[..n]).unwrap();
            assert_eq!(msg, format!("message {i}"));
            println!("Server: received '{msg}'");

            // Send reply
            let reply = format!("reply {i}");
            stream.write_all(reply.as_bytes()).expect("write");
        }
    });

    thread::sleep(std::time::Duration::from_millis(100));

    let mut client = RdmaStream::connect(&connect_addr).expect("connect");

    for i in 0..num_messages {
        let msg = format!("message {i}");
        client.write_all(msg.as_bytes()).expect("write");

        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).expect("read");
        let reply = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(reply, format!("reply {i}"));
        println!("Client: received '{reply}'");
    }

    server.join().expect("server panicked");
    println!("Multi-message test passed!");
}

#[test]
fn stream_large_transfer() {
    let (bind_addr, connect_addr) = test_addrs();
    let data_size = 32 * 1024; // 32 KiB (fits in one message)

    let server = thread::spawn(move || {
        let listener = RdmaListener::bind(&bind_addr).expect("bind");
        let mut stream = listener.accept().expect("accept");

        let mut received = Vec::new();
        let mut buf = [0u8; 65536];
        while received.len() < data_size {
            let n = stream.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }
        assert_eq!(received.len(), data_size);

        // Verify pattern
        for (i, b) in received.iter().enumerate() {
            assert_eq!(*b, (i % 256) as u8, "mismatch at byte {i}");
        }
        println!("Server: verified {data_size} bytes");
    });

    thread::sleep(std::time::Duration::from_millis(100));

    let mut client = RdmaStream::connect(&connect_addr).expect("connect");

    // Generate test pattern
    let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
    client.write_all(&data).expect("write");
    println!("Client: sent {data_size} bytes");

    server.join().expect("server panicked");
    println!("Large transfer test passed!");
}
