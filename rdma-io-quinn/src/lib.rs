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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

use rdma_io::async_cm::AsyncCmListener;
use rdma_io::transport::{RecvCompletion, Transport, TransportBuilder};

type ConnectionMap<T> = Arc<RwLock<HashMap<SocketAddr, Arc<Mutex<T>>>>>;

type AcceptFuture<T> = Pin<Box<dyn Future<Output = rdma_io::Result<(SocketAddr, T)>> + Send>>;

/// RDMA-backed UDP socket for Quinn endpoints.
///
/// Implements [`AsyncUdpSocket`] by multiplexing across per-peer transports.
/// Server-side accept is driven within [`poll_recv`](AsyncUdpSocket::poll_recv).
///
/// Generic over the transport builder — use [`SendRecvConfig`] for Send/Recv
/// or [`CreditRingConfig`] for ring buffer transport.
///
/// [`SendRecvConfig`]: rdma_io::send_recv_transport::SendRecvConfig
/// [`CreditRingConfig`]: rdma_io::credit_ring_transport::CreditRingConfig
///
/// Quinn 0.11 API: `poll_recv(&self)` + `try_send(&self)` + `create_io_poller(self: Arc<Self>)`.
/// All methods take `&self`, so internal state uses `Mutex` for interior mutability.
pub struct RdmaUdpSocket<B: TransportBuilder> {
    listener: Arc<AsyncCmListener>,
    connections: ConnectionMap<B::Transport>,
    local_addr: SocketAddr,
    builder: B,
    accept_state: Mutex<Option<AcceptFuture<B::Transport>>>,
    /// Waker from the last `poll_recv` that returned `Pending`.
    /// `connect_to` wakes this so Quinn's driver discovers the new connection.
    recv_waker: Mutex<Option<Waker>>,
    /// Transport whose send buffers were full on the last `try_send`.
    /// `poll_writable` waits for a send CQ completion on this transport
    /// instead of busy-spinning.
    send_blocked: Mutex<Option<Arc<Mutex<B::Transport>>>>,
    /// Consecutive accept errors. Reset on success; propagated after threshold.
    accept_errors: AtomicU32,
}

/// Poller for write-readiness on the RDMA socket.
///
/// Quinn calls `poll_writable` after `try_send` returns `WouldBlock`.
/// When a transport's send buffers are full, this waits for a send CQ
/// completion before returning `Ready`, avoiding a busy-spin.
pub struct RdmaUdpPoller<B: TransportBuilder> {
    socket: Arc<RdmaUdpSocket<B>>,
}

impl<B: TransportBuilder> fmt::Debug for RdmaUdpSocket<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdmaUdpSocket")
            .field("local_addr", &self.local_addr)
            .field("connections", &self.connections.read().unwrap().len())
            .finish()
    }
}

impl<B: TransportBuilder> fmt::Debug for RdmaUdpPoller<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdmaUdpPoller")
            .field("local_addr", &self.socket.local_addr)
            .finish()
    }
}

impl<B: TransportBuilder> RdmaUdpSocket<B> {
    /// Bind to a local address and start listening for RDMA connections.
    pub fn bind(addr: &SocketAddr, builder: B) -> rdma_io::Result<Self> {
        let listener = AsyncCmListener::bind(addr)?;
        let local_addr = listener
            .local_addr()
            .ok_or(rdma_io::Error::InvalidArg("no local address".into()))?;
        Ok(Self {
            listener: Arc::new(listener),
            connections: Arc::new(RwLock::new(HashMap::new())),
            local_addr,
            builder,
            accept_state: Mutex::new(None),
            recv_waker: Mutex::new(None),
            send_blocked: Mutex::new(None),
            accept_errors: AtomicU32::new(0),
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
    ///
    /// Can be called before or after the Quinn endpoint is created. If the
    /// endpoint already exists, `poll_recv` will be woken to discover the
    /// new connection.
    pub async fn connect_to(&self, addr: &SocketAddr) -> rdma_io::Result<()> {
        let transport = self.builder.connect(addr).await?;
        let peer = transport
            .peer_addr()
            .ok_or(rdma_io::Error::InvalidArg("no peer addr".into()))?;
        self.connections
            .write()
            .unwrap()
            .insert(peer, Arc::new(Mutex::new(transport)));
        // Wake poll_recv so Quinn's driver discovers the new connection.
        if let Some(waker) = self.recv_waker.lock().unwrap().take() {
            waker.wake();
        }
        Ok(())
    }

    /// Gracefully disconnect all peer connections and release RDMA resources.
    ///
    /// After calling `close`, `try_send` returns `NotConnected` and `poll_recv`
    /// returns `Pending` (no connections to poll). Call this before dropping the
    /// Quinn endpoint to ensure RDMA resources are released promptly instead of
    /// waiting for `Arc` reference counting.
    pub fn close(&self) {
        let mut connections = self.connections.write().unwrap();
        for (_addr, transport_arc) in connections.drain() {
            if let Ok(mut transport) = transport_arc.lock() {
                let _ = transport.disconnect();
            }
        }
        // Cancel any pending accept future.
        *self.accept_state.lock().unwrap() = None;
        *self.send_blocked.lock().unwrap() = None;
    }

    /// Maximum consecutive accept errors before propagating to caller.
    const MAX_ACCEPT_ERRORS: u32 = 10;

    /// Drive the accept state machine. Non-blocking, called from poll_recv.
    fn poll_accept(&self, cx: &mut Context<'_>) -> io::Result<()> {
        let mut accept = self.accept_state.lock().unwrap();
        loop {
            if let Some(fut) = accept.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Pending => return Ok(()),
                    Poll::Ready(Ok((addr, transport))) => {
                        self.accept_errors.store(0, Ordering::Relaxed);
                        tracing::debug!(%addr, "RDMA connection accepted");
                        self.connections
                            .write()
                            .unwrap()
                            .insert(addr, Arc::new(Mutex::new(transport)));
                        *accept = None;
                    }
                    Poll::Ready(Err(e)) => {
                        let n = self.accept_errors.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::warn!(error = %e, consecutive = n, "RDMA accept failed");
                        *accept = None;
                        if n >= Self::MAX_ACCEPT_ERRORS {
                            return Err(io::Error::other(format!(
                                "RDMA accept failed {n} consecutive times, last: {e}"
                            )));
                        }
                    }
                }
            } else {
                // Spawn a new accept future using the transport builder.
                let builder = self.builder.clone();
                let listener = Arc::clone(&self.listener);
                *accept = Some(Box::pin(async move {
                    let transport = builder.accept(&listener).await?;
                    let addr = transport
                        .peer_addr()
                        .ok_or(rdma_io::Error::InvalidArg("no peer addr".into()))?;
                    Ok((addr, transport))
                }));
            }
        }
    }
}

impl<B: TransportBuilder> AsyncUdpSocket for RdmaUdpSocket<B> {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(RdmaUdpPoller { socket: self })
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
            Ok(_) => {
                // All send buffers full — stash transport so poll_writable
                // can wait on its send CQ instead of busy-spinning.
                *self.send_blocked.lock().unwrap() = Some(Arc::clone(transport_arc));
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
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

                // poll_disconnect registers the CM fd waker so DREQ/disconnect
                // events wake poll_recv, and checks actual QP state.
                if transport.poll_disconnect(cx) {
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
                        Poll::Ready(Ok(_)) => {
                            // Credit-only batch (ring transport) — no data but
                            // internal credit state updated. Re-poll so the CQ
                            // registers a waker via poll_completions, otherwise
                            // future data on this CQ goes unnoticed.
                            continue;
                        }
                        Poll::Ready(Err(_)) => {
                            dead_addrs.push(*addr);
                            break;
                        }
                        Poll::Pending => break,
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
            // Save waker so connect_to can wake us when a new connection is added.
            *self.recv_waker.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

impl<B: TransportBuilder> UdpPoller for RdmaUdpPoller<B> {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let blocked = self.socket.send_blocked.lock().unwrap().clone();
        match blocked {
            Some(transport_arc) => {
                let mut transport = transport_arc.lock().unwrap();
                match transport.poll_send_completion(cx) {
                    Poll::Ready(_) => {
                        // Send buffer freed (or error) — clear blocked state.
                        // Quinn will retry try_send and discover any error there.
                        *self.socket.send_blocked.lock().unwrap() = None;
                        Poll::Ready(Ok(()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            // No blocked transport — send buffers available.
            None => Poll::Ready(Ok(())),
        }
    }
}

impl<B: TransportBuilder> Drop for RdmaUdpSocket<B> {
    fn drop(&mut self) {
        self.close();
    }
}
