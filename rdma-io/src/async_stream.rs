//! Async RDMA Stream — async `read` + `write` over RDMA SEND/RECV.
//!
//! Provides TCP-like async semantics over rdma_cm connections, built on
//! top of [`AsyncCq`] for completion-driven (non-spinning) I/O.
//!
//! # Buffer Architecture
//!
//! Each stream has one send buffer and one recv buffer (pre-registered MRs).
//! Send and recv use **separate CQs** so `poll_write` never blocks on recv
//! state and vice versa — this avoids deadlocks when both peers send
//! simultaneously (e.g., HTTP/2 connection setup).
//!
//! # Protocol
//!
//! Pure RDMA SEND/RECV with no application-level framing. Each `write()`
//! becomes one RDMA SEND; each `read()` consumes one RDMA RECV completion.

use std::io;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};
use tokio::io::unix::AsyncFd;

use crate::async_cm::{AsyncCmId, AsyncCmListener};
use crate::async_cq::{AsyncCq, CqPollState};
use crate::cm::{CmId, ConnParam, EventChannel, PortSpace};
use crate::comp_channel::CompletionChannel;
use crate::cq::CompletionQueue;
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;
use crate::tokio_notifier::TokioCqNotifier;
use crate::wc::WorkCompletion;
use crate::wr::QpType;

/// Default buffer size for stream send/recv (64 KiB).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// Number of pre-posted recv buffers.
///
/// iWARP (siw) has no RNR retry — missing recv buffers cause an immediate
/// DDP TERMINATE error. Multiple buffers absorb back-to-back sends from
/// the peer (e.g. HTTP/2 rapid frame bursts).
const NUM_RECV_BUFS: usize = 4;

/// WR IDs: 0..NUM_RECV_BUFS-1 for recv buffers, NUM_RECV_BUFS for send.
const SEND_WR_ID: u64 = NUM_RECV_BUFS as u64;

/// An async RDMA stream with `read` and `write` methods.
///
/// Created via [`AsyncRdmaStream::connect`] (client) or
/// [`AsyncRdmaListener::accept`] (server).
///
/// # Example
///
/// ```no_run
/// use rdma_io::async_stream::{AsyncRdmaListener, AsyncRdmaStream};
///
/// # async fn example() -> rdma_io::Result<()> {
/// // Server
/// let listener = AsyncRdmaListener::bind(&"0.0.0.0:9999".parse().unwrap())?;
/// let mut stream = listener.accept().await?;
/// let mut buf = [0u8; 1024];
/// let n = stream.read(&mut buf).await?;
///
/// // Client
/// let mut stream = AsyncRdmaStream::connect(&"10.0.0.1:9999".parse().unwrap()).await?;
/// stream.write(b"hello").await?;
/// # Ok(())
/// # }
/// ```
pub struct AsyncRdmaStream {
    // Drop order matters! Fields drop in declaration order.
    // MRs and poll state must be freed before RDMA teardown.
    /// Partially consumed recv: (buf_index, offset, total_len).
    recv_pending: Option<(usize, usize, usize)>,
    send_cq_state: CqPollState,
    recv_cq_state: CqPollState,
    /// In-flight send: (len,). None if send slot is free.
    write_pending: Option<usize>,
    disconnected: bool,
    /// Set when poll_read sees a flush error (QP entered ERROR state).
    /// Once set, poll_read always returns Ok(0) — the QP will never
    /// produce another recv completion.
    read_eof: bool,
    send_mr: OwnedMemoryRegion,
    recv_mrs: [OwnedMemoryRegion; NUM_RECV_BUFS],
    _pd: Arc<ProtectionDomain>,
    // RDMA teardown: QP first (needs CQs alive), then CQs, then CM.
    cmqp: crate::cm::CmQueuePair,
    send_cq: AsyncCq,
    recv_cq: AsyncCq,
    /// Async fd for CM event channel — registered alongside CQ fds so
    /// disconnect events wake poll_read/poll_write even when no CQ
    /// completions arrive (critical for rxe where CQ flush is unreliable).
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

// Safety: All interior types are Send-safe. The CmId raw pointers
// are guarded by libibverbs internal locking.
unsafe impl Send for AsyncRdmaStream {}

impl AsyncRdmaStream {
    /// Connect to a remote RDMA endpoint (client side).
    ///
    /// Uses async CM event notification — does not block the executor.
    pub async fn connect(addr: &SocketAddr) -> crate::Result<Self> {
        Self::connect_with_buf_size(addr, DEFAULT_BUF_SIZE).await
    }

    /// Connect with a custom buffer size.
    pub async fn connect_with_buf_size(addr: &SocketAddr, buf_size: usize) -> crate::Result<Self> {
        let async_cm = AsyncCmId::new(PortSpace::Tcp)?;
        async_cm.resolve_addr(None, addr, 2000).await?;
        async_cm.resolve_route(2000).await?;

        let ctx = async_cm
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = async_cm.alloc_pd()?;

        // Separate CQs for send and recv — avoids deadlock when both
        // sides send simultaneously (e.g. HTTP/2 connection setup).
        let send_ch = CompletionChannel::new(&ctx)?;
        let send_cq_raw = CompletionQueue::with_comp_channel(ctx.clone(), 2, &send_ch)?;
        let recv_ch = CompletionChannel::new(&ctx)?;
        let recv_cq_raw =
            CompletionQueue::with_comp_channel(ctx, NUM_RECV_BUFS as i32 + 1, &recv_ch)?;

        let cmqp = async_cm.create_qp_with_cq(
            &pd,
            &stream_qp_attr(),
            Some(&send_cq_raw),
            Some(&recv_cq_raw),
        )?;

        let send_mr = pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs: [OwnedMemoryRegion; NUM_RECV_BUFS] = std::array::from_fn(|_| {
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)
                .expect("failed to allocate recv MR")
        });

        // Pre-post all recv buffers before connect
        for (i, mr) in recv_mrs.iter().enumerate() {
            post_recv_raw(cmqp.as_raw(), mr, i as u64)?;
        }

        async_cm.connect(&ConnParam::default()).await?;

        // Create async CQs AFTER connection is established
        let send_notifier = TokioCqNotifier::new(send_ch.fd()).map_err(crate::Error::Verbs)?;
        let send_cq = AsyncCq::new(send_cq_raw, send_ch, Box::new(send_notifier));
        let recv_notifier = TokioCqNotifier::new(recv_ch.fd()).map_err(crate::Error::Verbs)?;
        let recv_cq = AsyncCq::new(recv_cq_raw, recv_ch, Box::new(recv_notifier));

        let (event_channel, cm_id) = async_cm.into_parts();

        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        Ok(Self {
            cmqp,
            send_cq,
            recv_cq,
            cm_async_fd,
            cm_id,
            event_channel,
            _pd: pd,
            send_mr,
            recv_mrs,
            recv_pending: None,
            send_cq_state: CqPollState::default(),
            recv_cq_state: CqPollState::default(),
            write_pending: None,
            disconnected: false,
            read_eof: false,
        })
    }

    /// Read data from the stream asynchronously.
    ///
    /// Returns the number of bytes read. Returns `Ok(0)` on disconnect (EOF).
    ///
    /// Delegates to the [`futures_io::AsyncRead`] implementation.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_read(cx, buf)).await
    }

    /// Write data to the stream asynchronously.
    ///
    /// Returns the number of bytes written (bounded by buffer size).
    ///
    /// Delegates to the [`futures_io::AsyncWrite`] implementation.
    pub async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_write(cx, data)).await
    }

    /// Write all data to the stream, looping if necessary.
    pub async fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = self.write(data).await?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Disconnect the stream gracefully.
    ///
    /// Drains any pending send completion, disconnects, and awaits the
    /// `DISCONNECTED` event from the peer (analogous to TCP FIN/FIN-ACK).
    pub async fn shutdown(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_close(cx)).await
    }

    /// Get the peer's socket address (remote end of the connection).
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.cm_id.peer_addr()
    }

    /// Get the local socket address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.cm_id.local_addr()
    }
}

impl Drop for AsyncRdmaStream {
    fn drop(&mut self) {
        // Best-effort disconnect so the peer sees DREQ promptly.
        // Skip if poll_close/shutdown already disconnected gracefully.
        if !self.disconnected {
            let _ = self.cm_id.disconnect();
        }
    }
}

// --- Private helpers for poll-based trait impls ---

impl AsyncRdmaStream {
    /// Process a recv work completion, copying data into `buf`.
    fn process_recv_wc(&mut self, wc: &WorkCompletion, buf: &mut [u8]) -> io::Result<usize> {
        let buf_idx = wc.wr_id() as usize;
        let byte_len = wc.byte_len() as usize;
        if byte_len == 0 {
            return Ok(0);
        }
        let copy_len = byte_len.min(buf.len());
        buf[..copy_len].copy_from_slice(&self.recv_mrs[buf_idx].as_slice()[..copy_len]);
        if copy_len < byte_len {
            self.recv_pending = Some((buf_idx, copy_len, byte_len));
        } else {
            post_recv_raw(self.cmqp.as_raw(), &self.recv_mrs[buf_idx], buf_idx as u64)
                .map_err(io::Error::other)?;
        }
        Ok(copy_len)
    }

    /// Check if the peer has disconnected by probing the CM event channel
    /// and QP state. Sets `read_eof` if disconnection is detected.
    ///
    /// This is the primary disconnect detection mechanism. On rxe (RoCE),
    /// CQ flush completions may not arrive (or their EPOLLET notifications
    /// may be lost), and the QP may linger in RTS until the CM event is
    /// consumed. Checking the CM event channel directly is reliable across
    /// all providers.
    fn check_disconnect(&mut self) -> bool {
        if self.read_eof {
            return true;
        }
        // Check CM event channel for DISCONNECTED (non-blocking).
        match self.event_channel.try_get_event() {
            Ok(ev) => {
                let etype = ev.event_type();
                ev.ack();
                if etype == crate::cm::CmEventType::Disconnected {
                    self.read_eof = true;
                    return true;
                }
                // Other unexpected events — treat as disconnect.
                self.read_eof = true;
                true
            }
            Err(crate::Error::WouldBlock) => {
                // No CM event — fall back to QP state check.
                if is_qp_dead(self.cmqp.as_raw()) {
                    self.read_eof = true;
                    return true;
                }
                false
            }
            Err(_) => {
                // Event channel error — connection is dead.
                self.read_eof = true;
                true
            }
        }
    }

    /// Register the task waker on the CM event channel fd.
    ///
    /// This ensures that disconnect events (DREQ → DISCONNECTED) wake up
    /// poll_read/poll_write even when the CQ fd never becomes readable.
    /// Critical for rxe where CQ flush completions are unreliable.
    ///
    /// If the CM fd is already readable, drains readiness and calls
    /// `check_disconnect()`. May set `read_eof` as a side effect.
    fn register_cm_waker(&mut self, cx: &mut Context<'_>) {
        loop {
            match self.cm_async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(mut guard)) => {
                    guard.clear_ready();
                    if self.check_disconnect() {
                        return;
                    }
                    // Spurious readiness or non-disconnect event consumed;
                    // loop to re-register the waker.
                }
                Poll::Pending => return, // waker registered
                Poll::Ready(Err(_)) => {
                    self.read_eof = true;
                    return;
                }
            }
        }
    }
}

// --- futures::io trait implementations ---

impl futures_io::AsyncRead for AsyncRdmaStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if buf.is_empty() || this.read_eof {
            return Poll::Ready(Ok(0));
        }

        // 1. Return buffered recv data (partial read from previous completion)
        if let Some((buf_idx, offset, total_len)) = this.recv_pending {
            let remaining = total_len - offset;
            let copy_len = remaining.min(buf.len());
            buf[..copy_len]
                .copy_from_slice(&this.recv_mrs[buf_idx].as_slice()[offset..offset + copy_len]);
            if copy_len < remaining {
                this.recv_pending = Some((buf_idx, offset + copy_len, total_len));
            } else {
                this.recv_pending = None;
                post_recv_raw(this.cmqp.as_raw(), &this.recv_mrs[buf_idx], buf_idx as u64)
                    .map_err(io::Error::other)?;
            }
            return Poll::Ready(Ok(copy_len));
        }

        // 2. Poll recv CQ for completion (only recv completions appear here)
        let mut wc = [WorkCompletion::default(); 1];
        loop {
            let n = match this
                .recv_cq
                .poll_completions(cx, &mut this.recv_cq_state, &mut wc)
            {
                Poll::Pending => {
                    // Register waker on CM event channel fd so disconnect
                    // events wake this task even if CQ fd never triggers.
                    this.register_cm_waker(cx);
                    if this.read_eof {
                        return Poll::Ready(Ok(0)); // EOF
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Ready(Ok(n)) => n,
            };
            if n > 0 {
                let wc = &wc[0];
                if !wc.is_success() {
                    if wc.status_raw() == rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR {
                        this.read_eof = true;
                        return Poll::Ready(Ok(0)); // EOF
                    }
                    return Poll::Ready(Err(io::Error::other(format!(
                        "recv WC error: status={:?} wr_id={}",
                        wc.status(),
                        wc.wr_id()
                    ))));
                }
                return Poll::Ready(this.process_recv_wc(wc, buf));
            }
        }
    }
}

impl futures_io::AsyncWrite for AsyncRdmaStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // QP is in ERROR state — writes will never complete.
        if this.read_eof {
            this.write_pending = None;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection closed",
            )));
        }

        // Post send if not already in progress
        if this.write_pending.is_none() {
            let send_len = buf.len().min(this.send_mr.as_slice().len());
            this.send_mr.as_mut_slice()[..send_len].copy_from_slice(&buf[..send_len]);

            post_send_raw(this.cmqp.as_raw(), &this.send_mr, 0, send_len, SEND_WR_ID)
                .map_err(io::Error::other)?;
            this.write_pending = Some(send_len);
        }

        let len = this.write_pending.unwrap();

        // Poll send CQ for completion (only send completions appear here)
        let mut wc = [WorkCompletion::default(); 1];
        loop {
            let n = match this
                .send_cq
                .poll_completions(cx, &mut this.send_cq_state, &mut wc)
            {
                Poll::Pending => {
                    // Register waker on CM event channel fd so disconnect
                    // events wake this task even if CQ fd never triggers.
                    this.register_cm_waker(cx);
                    if this.read_eof {
                        this.write_pending = None;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "connection closed",
                        )));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    this.write_pending = None;
                    return Poll::Ready(Err(io::Error::other(e)));
                }
                Poll::Ready(Ok(n)) => n,
            };
            if n > 0 {
                if !wc[0].is_success() {
                    this.write_pending = None;
                    return Poll::Ready(Err(io::Error::other(format!(
                        "send WC error: status={:?}",
                        wc[0].status()
                    ))));
                }
                this.write_pending = None;
                return Poll::Ready(Ok(len));
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // RDMA sends are flushed by the HCA — no application-level buffering.
        Poll::Ready(Ok(()))
    }

    /// Drains pending send completion, disconnects, and awaits the peer's
    /// DISCONNECTED event for a fully graceful close.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // QP is in ERROR state — peer already disconnected. Skip graceful
        // close; the Drop impl will handle cleanup.
        if this.read_eof {
            this.write_pending = None;
            this.disconnected = true;
            return Poll::Ready(Ok(()));
        }

        // Phase 1: Drain pending send completion before disconnecting.
        if this.write_pending.is_some() {
            let mut wc_buf = [WorkCompletion::default(); 1];
            loop {
                let n =
                    match this
                        .send_cq
                        .poll_completions(cx, &mut this.send_cq_state, &mut wc_buf)
                    {
                        Poll::Pending => {
                            this.register_cm_waker(cx);
                            if this.read_eof {
                                this.write_pending = None;
                                this.disconnected = true;
                                return Poll::Ready(Ok(()));
                            }
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => {
                            this.write_pending = None;
                            return Poll::Ready(Err(io::Error::other(e)));
                        }
                        Poll::Ready(Ok(n)) => n,
                    };
                if n > 0 {
                    this.write_pending = None;
                    break;
                }
            }
        }

        // Phase 2: Send DREQ.
        // Note: register_cm_waker/check_disconnect in Phase 1 or
        // poll_read/poll_write may have already consumed the DISCONNECTED
        // event and set read_eof.
        if this.read_eof {
            this.write_pending = None;
            this.disconnected = true;
            return Poll::Ready(Ok(()));
        }
        if !this.disconnected {
            // disconnect() may fail if the connection is already being torn
            // down. Check CM events / QP state to distinguish real errors.
            if let Err(e) = this.cm_id.disconnect() {
                if this.check_disconnect() {
                    this.disconnected = true;
                    return Poll::Ready(Ok(()));
                }
                return Poll::Ready(Err(io::Error::other(e)));
            }
            this.disconnected = true;
        }

        // Phase 3: Await DISCONNECTED event from peer.
        // Uses the persistent cm_async_fd — same fd as register_cm_waker.
        loop {
            let mut guard = match this.cm_async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(io::Error::other(e)));
                }
                Poll::Pending => {
                    // DISCONNECTED may never arrive (peer crashed, no DREP).
                    // Check QP state as fallback.
                    if is_qp_dead(this.cmqp.as_raw()) {
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Pending;
                }
            };
            match this.event_channel.try_get_event() {
                Ok(ev) => {
                    let etype = ev.event_type();
                    ev.ack();
                    if etype == crate::cm::CmEventType::Disconnected {
                        return Poll::Ready(Ok(()));
                    }
                    // Unexpected event — treat as disconnect.
                    return Poll::Ready(Ok(()));
                }
                Err(crate::Error::WouldBlock) => {
                    guard.clear_ready();
                    continue;
                }
                Err(e) => {
                    return Poll::Ready(Err(io::Error::other(e)));
                }
            }
        }
    }
}

/// An async RDMA listener, analogous to `TcpListener`.
///
/// Binds to a local address and accepts incoming RDMA connections,
/// returning an [`AsyncRdmaStream`] for each. Built on [`AsyncCmListener`].
pub struct AsyncRdmaListener {
    inner: AsyncCmListener,
    buf_size: usize,
}

impl AsyncRdmaListener {
    /// Bind to a local address and start listening.
    pub fn bind(addr: &SocketAddr) -> crate::Result<Self> {
        Self::bind_with_buf_size(addr, DEFAULT_BUF_SIZE)
    }

    /// Bind with a custom buffer size for accepted streams.
    pub fn bind_with_buf_size(addr: &SocketAddr, buf_size: usize) -> crate::Result<Self> {
        let inner = AsyncCmListener::bind(addr)?;
        Ok(Self { inner, buf_size })
    }

    /// Accept an incoming connection, returning an [`AsyncRdmaStream`].
    ///
    /// Uses async CM event notification — does not block the executor.
    pub async fn accept(&self) -> crate::Result<AsyncRdmaStream> {
        // Phase 1: await CONNECT_REQUEST
        let conn_id = self.inner.get_request().await?;

        // Set up QP resources on the new CM ID
        let ctx = conn_id
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = conn_id.alloc_pd()?;

        // Separate CQs for send and recv
        let send_ch = CompletionChannel::new(&ctx)?;
        let send_cq_raw = CompletionQueue::with_comp_channel(ctx.clone(), 2, &send_ch)?;
        let recv_ch = CompletionChannel::new(&ctx)?;
        let recv_cq_raw =
            CompletionQueue::with_comp_channel(ctx, NUM_RECV_BUFS as i32 + 1, &recv_ch)?;

        let cmqp = conn_id.create_qp_with_cq(
            &pd,
            &stream_qp_attr(),
            Some(&send_cq_raw),
            Some(&recv_cq_raw),
        )?;

        let send_mr = pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs: [OwnedMemoryRegion; NUM_RECV_BUFS] = std::array::from_fn(|_| {
            pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)
                .expect("failed to allocate recv MR")
        });

        // Pre-post all recv buffers before accept handshake
        for (i, mr) in recv_mrs.iter().enumerate() {
            post_recv_raw(cmqp.as_raw(), mr, i as u64)?;
        }

        // Phase 2: accept handshake + await ESTABLISHED + migrate
        let async_cm = self
            .inner
            .complete_accept(conn_id, &ConnParam::default())
            .await?;

        // Create async CQs AFTER connection is established
        let send_notifier = TokioCqNotifier::new(send_ch.fd()).map_err(crate::Error::Verbs)?;
        let send_cq = AsyncCq::new(send_cq_raw, send_ch, Box::new(send_notifier));
        let recv_notifier = TokioCqNotifier::new(recv_ch.fd()).map_err(crate::Error::Verbs)?;
        let recv_cq = AsyncCq::new(recv_cq_raw, recv_ch, Box::new(recv_notifier));

        let (event_channel, cm_id) = async_cm.into_parts();

        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        Ok(AsyncRdmaStream {
            cmqp,
            send_cq,
            recv_cq,
            cm_async_fd,
            cm_id,
            event_channel,
            _pd: pd,
            send_mr,
            recv_mrs,
            recv_pending: None,
            send_cq_state: CqPollState::default(),
            recv_cq_state: CqPollState::default(),
            write_pending: None,
            disconnected: false,
            read_eof: false,
        })
    }
}

// --- Helpers ---

fn stream_qp_attr() -> QpInitAttr {
    QpInitAttr {
        qp_type: QpType::Rc,
        max_send_wr: 2,
        max_recv_wr: NUM_RECV_BUFS as u32 + 1,
        max_send_sge: 1,
        max_recv_sge: 1,
        max_inline_data: 0,
        sq_sig_all: true,
    }
}

/// Post a recv WR using raw ibverbs.
fn post_recv_raw(
    qp: *mut rdma_io_sys::ibverbs::ibv_qp,
    mr: &OwnedMemoryRegion,
    wr_id: u64,
) -> crate::Result<()> {
    let mut sge = rdma_io_sys::ibverbs::ibv_sge {
        addr: unsafe { (*mr.as_raw()).addr as u64 },
        length: mr.as_slice().len() as u32,
        lkey: mr.lkey(),
    };
    let mut wr = rdma_io_sys::ibverbs::ibv_recv_wr {
        wr_id,
        sg_list: &mut sge,
        num_sge: 1,
        ..Default::default()
    };
    let mut bad_wr: *mut rdma_io_sys::ibverbs::ibv_recv_wr = std::ptr::null_mut();
    let ret = unsafe { rdma_io_sys::wrapper::rdma_wrap_ibv_post_recv(qp, &mut wr, &mut bad_wr) };
    if ret != 0 {
        return Err(crate::Error::Verbs(io::Error::from_raw_os_error(-ret)));
    }
    Ok(())
}

/// Post a signaled send WR using raw ibverbs.
fn post_send_raw(
    qp: *mut rdma_io_sys::ibverbs::ibv_qp,
    mr: &OwnedMemoryRegion,
    offset: usize,
    len: usize,
    wr_id: u64,
) -> crate::Result<()> {
    let mut sge = rdma_io_sys::ibverbs::ibv_sge {
        addr: unsafe { (*mr.as_raw()).addr as u64 } + offset as u64,
        length: len as u32,
        lkey: mr.lkey(),
    };
    let mut wr = rdma_io_sys::ibverbs::ibv_send_wr {
        wr_id,
        sg_list: &mut sge,
        num_sge: 1,
        opcode: rdma_io_sys::ibverbs::IBV_WR_SEND,
        send_flags: rdma_io_sys::ibverbs::IBV_SEND_SIGNALED,
        ..Default::default()
    };
    let mut bad_wr: *mut rdma_io_sys::ibverbs::ibv_send_wr = std::ptr::null_mut();
    let ret = unsafe { rdma_io_sys::wrapper::rdma_wrap_ibv_post_send(qp, &mut wr, &mut bad_wr) };
    if ret != 0 {
        return Err(crate::Error::Verbs(io::Error::from_raw_os_error(-ret)));
    }
    Ok(())
}

/// Check if a QP is no longer operational (any state other than RTS).
///
/// Used as a safety net to detect dead connections when CQ notifications
/// may have been lost (e.g. due to EPOLLET edge-triggered races).
///
/// After rdma_disconnect, the QP may transition through SQD or SQE before
/// reaching ERROR. On rxe (RoCE), this transition can take significant time.
/// Any non-RTS state means data-path operations will never complete normally.
fn is_qp_dead(qp: *mut rdma_io_sys::ibverbs::ibv_qp) -> bool {
    unsafe {
        let mut attr: rdma_io_sys::ibverbs::ibv_qp_attr = std::mem::zeroed();
        let mut init_attr: rdma_io_sys::ibverbs::ibv_qp_init_attr = std::mem::zeroed();
        let ret = rdma_io_sys::ibverbs::ibv_query_qp(
            qp,
            &mut attr,
            rdma_io_sys::ibverbs::IBV_QP_STATE as i32,
            &mut init_attr,
        );
        ret == 0 && attr.qp_state != rdma_io_sys::ibverbs::IBV_QPS_RTS
    }
}
