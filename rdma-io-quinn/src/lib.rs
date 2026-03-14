//! Quinn QUIC over RDMA — [`AsyncUdpSocket`] implementation.
//!
//! Bridges Quinn's datagram socket abstraction to RDMA via the
//! [`Transport`](rdma_io::transport::Transport) trait.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

use rdma_io::async_cm::AsyncCmListener;
use rdma_io::rdma_transport::{RdmaTransport, TransportConfig};
use rdma_io::transport::{RecvCompletion, Transport};

type ConnectionMap = Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<RdmaTransport>>>>>;

type AcceptFuture =
    Pin<Box<dyn Future<Output = rdma_io::Result<(SocketAddr, RdmaTransport)>> + Send>>;

/// RDMA-backed UDP socket for Quinn endpoints.
///
/// Implements [`AsyncUdpSocket`] by multiplexing across per-peer transports.
/// Server-side accept is driven within [`poll_recv`](AsyncUdpSocket::poll_recv).
///
/// Quinn 0.11 API: `poll_recv(&self)` + `try_send(&self)` + `create_io_poller(self: Arc<Self>)`.
/// All methods take `&self`, so internal state uses `Mutex` for interior mutability.
pub struct RdmaUdpSocket {
    listener: Arc<AsyncCmListener>,
    connections: ConnectionMap,
    local_addr: SocketAddr,
    config: TransportConfig,
    accept_state: Mutex<Option<AcceptFuture>>,
}

/// Poller for write-readiness on the RDMA socket.
///
/// Quinn calls `poll_writable` after `try_send` returns `WouldBlock`.
/// For RDMA, sends rarely block (hardware RNR handles backpressure),
/// so this always returns Ready.
pub struct RdmaUdpPoller;

impl fmt::Debug for RdmaUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdmaUdpSocket")
            .field("local_addr", &self.local_addr)
            .field("connections", &self.connections.read().unwrap().len())
            .finish()
    }
}

impl fmt::Debug for RdmaUdpPoller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RdmaUdpPoller")
    }
}

impl RdmaUdpSocket {
    /// Bind to a local address and start listening for RDMA connections.
    pub fn bind(addr: &SocketAddr) -> rdma_io::Result<Self> {
        Self::bind_with_config(addr, TransportConfig::datagram())
    }

    /// Bind with a custom transport configuration.
    pub fn bind_with_config(addr: &SocketAddr, config: TransportConfig) -> rdma_io::Result<Self> {
        let listener = AsyncCmListener::bind(addr)?;
        let local_addr = listener
            .local_addr()
            .ok_or(rdma_io::Error::InvalidArg("no local address".into()))?;
        Ok(Self {
            listener: Arc::new(listener),
            connections: Arc::new(RwLock::new(HashMap::new())),
            local_addr,
            config,
            accept_state: Mutex::new(None),
        })
    }

    /// Get the local bound address. Useful for ephemeral port discovery.
    pub fn bound_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Pre-establish an RDMA connection to a peer.
    ///
    /// Must be called before Quinn sends to this address. RDMA connections
    /// are point-to-point, unlike UDP sockets — they must be established
    /// before data can flow.
    pub async fn connect_to(
        &self,
        addr: &SocketAddr,
        config: TransportConfig,
    ) -> rdma_io::Result<()> {
        let transport = RdmaTransport::connect(addr, config).await?;
        let peer = transport
            .peer_addr()
            .ok_or(rdma_io::Error::InvalidArg("no peer addr".into()))?;
        self.connections
            .write()
            .unwrap()
            .insert(peer, Arc::new(Mutex::new(transport)));
        Ok(())
    }

    /// Drive the accept state machine. Non-blocking, called from poll_recv.
    fn poll_accept(&self, cx: &mut Context<'_>) -> io::Result<()> {
        let mut accept = self.accept_state.lock().unwrap();
        loop {
            if let Some(fut) = accept.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Ok(()),
                    Poll::Ready(Ok((addr, transport))) => {
                        self.connections
                            .write()
                            .unwrap()
                            .insert(addr, Arc::new(Mutex::new(transport)));
                        *accept = None;
                    }
                    Poll::Ready(Err(e)) => {
                        eprintln!("RDMA accept failed: {e}");
                        *accept = None;
                        return Ok(());
                    }
                }
            } else {
                match self.listener.poll_get_request(cx) {
                    Poll::Pending => return Ok(()),
                    Poll::Ready(Ok(conn_id)) => {
                        let config = self.config.clone();
                        let listener = Arc::clone(&self.listener);
                        *accept = Some(Box::pin(async move {
                            let transport =
                                RdmaTransport::complete_accept(conn_id, &listener, config).await?;
                            let addr = transport
                                .peer_addr()
                                .ok_or(rdma_io::Error::InvalidArg("no peer addr".into()))?;
                            Ok((addr, transport))
                        }));
                    }
                    Poll::Ready(Err(e)) => {
                        eprintln!("RDMA listen error: {e}");
                        return Ok(());
                    }
                }
            }
        }
    }
}

impl AsyncUdpSocket for RdmaUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(RdmaUdpPoller)
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        let connections = self.connections.read().unwrap();
        let transport_arc = connections.get(&transmit.destination).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("no RDMA connection to {}", transmit.destination),
            )
        })?;
        let mut transport = transport_arc.lock().unwrap();

        // Try to drain any completed sends first (frees buffer slots).
        // Use a no-op waker since we don't need to register for notification.
        let noop_waker = std::task::Waker::noop();
        let mut noop_cx = Context::from_waker(noop_waker);
        let _ = transport.poll_send_completion(&mut noop_cx);

        match transport.send_copy(transmit.contents) {
            Ok(n) if n > 0 => Ok(()),
            Ok(_) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            Err(e) => Err(io::Error::other(e)),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut count = 0;

        // 1. Accept new connections
        self.poll_accept(cx)?;

        // 2. Poll all transports
        let mut dead_addrs = Vec::new();
        {
            let connections = self.connections.read().unwrap();
            for (addr, transport_arc) in connections.iter() {
                let mut transport = transport_arc.lock().unwrap();

                if transport.is_qp_dead() {
                    dead_addrs.push(*addr);
                    continue;
                }

                let mut completions = [RecvCompletion::default(); 8];
                while count < bufs.len() {
                    match transport.poll_recv(cx, &mut completions) {
                        Poll::Ready(Ok(n)) if n > 0 => {
                            for c in &completions[..n] {
                                if count >= bufs.len() {
                                    break;
                                }
                                let data = transport.recv_buf(c.buf_idx);
                                bufs[count][..c.byte_len].copy_from_slice(&data[..c.byte_len]);
                                meta[count] = RecvMeta {
                                    addr: *addr,
                                    len: c.byte_len,
                                    stride: c.byte_len,
                                    ecn: None,
                                    dst_ip: Some(self.local_addr.ip()),
                                };
                                transport.repost_recv(c.buf_idx).map_err(io::Error::other)?;
                                count += 1;
                            }
                        }
                        Poll::Ready(Err(_)) => {
                            dead_addrs.push(*addr);
                            break;
                        }
                        _ => break,
                    }
                }
            }
        }

        // 3. Cleanup dead connections
        if !dead_addrs.is_empty() {
            let mut connections = self.connections.write().unwrap();
            for addr in dead_addrs {
                connections.remove(&addr);
            }
        }

        if count > 0 {
            Poll::Ready(Ok(count))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

impl UdpPoller for RdmaUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // RDMA send buffers are almost always available (hardware RNR handles
        // backpressure). If try_send returned WouldBlock, a send completion
        // will free a buffer shortly — always report writable.
        Poll::Ready(Ok(()))
    }
}
