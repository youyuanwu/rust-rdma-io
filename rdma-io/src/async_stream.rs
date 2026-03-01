//! Async RDMA Stream — async `read` + `write` over RDMA SEND/RECV.
//!
//! Provides TCP-like async semantics over rdma_cm connections, built on
//! top of [`AsyncQp`] for completion-driven (non-spinning) I/O.
//!
//! # Buffer Architecture
//!
//! Same as the sync `RdmaStream`:
//! - **Send buffer**: Pre-registered MR for outgoing data.
//! - **Recv buffers**: Two pre-registered MRs (double-buffered).
//!
//! # Protocol
//!
//! Pure RDMA SEND/RECV with no application-level framing. Each `write()`
//! becomes one RDMA SEND; each `read()` consumes one RDMA RECV completion.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};

use crate::async_cm::{AsyncCmId, AsyncCmListener};
use crate::async_cq::{AsyncCq, CqPollState};
use crate::async_qp::AsyncQp;
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

/// Number of recv buffers (double-buffered).
const NUM_RECV_BUFS: usize = 2;

/// Base WR ID for send operations (recv uses buf_idx 0..NUM_RECV_BUFS).
const SEND_WR_ID: u64 = 100;

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
    // Keep CM resources alive — the QP pointer in AsyncQp borrows from cm_id.
    _event_channel: EventChannel,
    cm_id: CmId,
    _pd: Arc<ProtectionDomain>,
    aqp: AsyncQp,
    send_mr: OwnedMemoryRegion,
    recv_mrs: [OwnedMemoryRegion; NUM_RECV_BUFS],
    /// Buffered recv data: (buffer_index, offset, length).
    recv_pending: Option<(usize, usize, usize)>,
    /// Next send WR ID (incremented per send for uniqueness).
    send_wr_seq: u64,
    /// CQ poll state for poll-based AsyncRead/AsyncWrite.
    cq_state: CqPollState,
    /// Recv completions encountered during poll_write (drained by poll_read).
    stashed_recv: VecDeque<WorkCompletion>,
    /// Send completions encountered during poll_read (drained by poll_write).
    stashed_send: VecDeque<WorkCompletion>,
    /// In-progress write: (wr_id, byte_len). Set when send is posted, cleared on completion.
    write_pending: Option<(u64, usize)>,
}

// Safety: All interior types are Send-safe. The CmId/AsyncQp raw pointers
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
        // Step-by-step connect: we need to create QP between resolve_route and connect
        let async_cm = AsyncCmId::new(PortSpace::Tcp)?;
        async_cm.resolve_addr(None, addr, 2000).await?;
        async_cm.resolve_route(2000).await?;

        let ctx = async_cm
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = async_cm.alloc_pd()?;
        let comp_ch = CompletionChannel::new(&ctx)?;
        let cq = CompletionQueue::with_comp_channel(ctx, 32, &comp_ch)?;

        async_cm.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs = [
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
        ];

        // Pre-post recv buffers before connect
        for (i, mr) in recv_mrs.iter().enumerate() {
            post_recv_raw(async_cm.qp_raw(), mr, i as u64)?;
        }

        async_cm.connect(&ConnParam::default()).await?;

        // Extract QP pointer AFTER the last await
        let qp = async_cm.qp_raw();

        // Create async CQ AFTER connection is established
        let notifier = TokioCqNotifier::new(comp_ch.fd()).map_err(crate::Error::Verbs)?;
        let async_cq = AsyncCq::new(cq, comp_ch, Box::new(notifier));
        let aqp = unsafe { AsyncQp::new(qp, async_cq) };

        let (event_channel, cm_id) = async_cm.into_parts();

        Ok(Self {
            _event_channel: event_channel,
            cm_id,
            _pd: pd,
            aqp,
            send_mr,
            recv_mrs,
            recv_pending: None,
            send_wr_seq: SEND_WR_ID,
            cq_state: CqPollState::default(),
            stashed_recv: VecDeque::new(),
            stashed_send: VecDeque::new(),
            write_pending: None,
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
    pub fn shutdown(&self) -> crate::Result<()> {
        self.cm_id.disconnect()
    }
}

impl Drop for AsyncRdmaStream {
    fn drop(&mut self) {
        if let Err(e) = self.cm_id.disconnect() {
            tracing::error!("AsyncRdmaStream disconnect failed: {e}");
        }
    }
}

// --- Private helpers for poll-based trait impls ---

impl AsyncRdmaStream {
    /// Process a recv work completion, copying data into `buf`.
    fn process_recv_wc(&mut self, wc: &WorkCompletion, buf: &mut [u8]) -> io::Result<usize> {
        let byte_len = wc.byte_len() as usize;
        if byte_len == 0 {
            return Ok(0);
        }
        let bidx = wc.wr_id() as usize;
        let copy_len = byte_len.min(buf.len());
        buf[..copy_len].copy_from_slice(&self.recv_mrs[bidx].as_slice()[..copy_len]);
        if copy_len < byte_len {
            self.recv_pending = Some((bidx, copy_len, byte_len));
        } else {
            post_recv_raw(self.cm_id.qp_raw(), &self.recv_mrs[bidx], wc.wr_id())
                .map_err(io::Error::other)?;
        }
        Ok(copy_len)
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

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // 1. Return buffered recv data
        if let Some((buf_idx, offset, total_len)) = this.recv_pending {
            let remaining = total_len - offset;
            let copy_len = remaining.min(buf.len());
            buf[..copy_len]
                .copy_from_slice(&this.recv_mrs[buf_idx].as_slice()[offset..offset + copy_len]);
            if copy_len < remaining {
                this.recv_pending = Some((buf_idx, offset + copy_len, total_len));
            } else {
                this.recv_pending = None;
                post_recv_raw(this.cm_id.qp_raw(), &this.recv_mrs[buf_idx], buf_idx as u64)
                    .map_err(io::Error::other)?;
            }
            return Poll::Ready(Ok(copy_len));
        }

        // 2. Check stashed recv completion (from a poll_write that encountered recv)
        if let Some(wc) = this.stashed_recv.pop_front() {
            return Poll::Ready(this.process_recv_wc(&wc, buf));
        }

        // 3. Poll CQ for recv completions
        let mut wc_buf = [WorkCompletion::default(); 4];
        loop {
            let n = match this
                .aqp
                .async_cq()
                .poll_completions(cx, &mut this.cq_state, &mut wc_buf)
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Ready(Ok(n)) => n,
            };
            for wc_item in &wc_buf[..n] {
                if !wc_item.is_success() {
                    if wc_item.status_raw() == rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR {
                        return Poll::Ready(Ok(0)); // EOF
                    }
                    return Poll::Ready(Err(io::Error::other(format!(
                        "WC error: status={:?}",
                        wc_item.status()
                    ))));
                }
                if wc_item.wr_id() < NUM_RECV_BUFS as u64 {
                    return Poll::Ready(this.process_recv_wc(wc_item, buf));
                }
                // Stash send completions for later poll_write
                this.stashed_send.push_back(*wc_item);
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

        // Post send if not already in progress
        if this.write_pending.is_none() {
            let send_len = buf.len().min(this.send_mr.as_slice().len());
            this.send_mr.as_mut_slice()[..send_len].copy_from_slice(&buf[..send_len]);

            let wr_id = this.send_wr_seq;
            this.send_wr_seq += 1;

            this.aqp
                .post_send_signaled(&this.send_mr, 0, send_len, wr_id)
                .map_err(io::Error::other)?;
            this.write_pending = Some((wr_id, send_len));
        }

        let (wr_id, len) = this.write_pending.unwrap();

        // Check stashed send completions (from a poll_read that encountered send)
        while let Some(wc) = this.stashed_send.pop_front() {
            if !wc.is_success() {
                this.write_pending = None;
                return Poll::Ready(Err(io::Error::other(format!(
                    "send WC error: status={:?}",
                    wc.status()
                ))));
            }
            if wc.wr_id() == wr_id {
                this.write_pending = None;
                return Poll::Ready(Ok(len));
            }
        }

        // Poll CQ for send completion
        let mut wc_buf = [WorkCompletion::default(); 4];
        loop {
            let n = match this
                .aqp
                .async_cq()
                .poll_completions(cx, &mut this.cq_state, &mut wc_buf)
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => {
                    this.write_pending = None;
                    return Poll::Ready(Err(io::Error::other(e)));
                }
                Poll::Ready(Ok(n)) => n,
            };
            for wc_item in &wc_buf[..n] {
                if !wc_item.is_success() {
                    this.write_pending = None;
                    return Poll::Ready(Err(io::Error::other(format!(
                        "send WC error: status={:?}",
                        wc_item.status()
                    ))));
                }
                if wc_item.wr_id() == wr_id {
                    this.write_pending = None;
                    return Poll::Ready(Ok(len));
                }
                // Stash recv completions for later poll_read
                if wc_item.wr_id() < NUM_RECV_BUFS as u64 {
                    this.stashed_recv.push_back(*wc_item);
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // RDMA sends are flushed by the HCA — no application-level buffering.
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut()
            .cm_id
            .disconnect()
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(()))
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
        let comp_ch = CompletionChannel::new(&ctx)?;
        let cq = CompletionQueue::with_comp_channel(ctx, 32, &comp_ch)?;

        conn_id.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs = [
            pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?,
            pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?,
        ];
        for (i, mr) in recv_mrs.iter().enumerate() {
            post_recv_raw(conn_id.qp_raw(), mr, i as u64)?;
        }

        // Phase 2: accept handshake + await ESTABLISHED + migrate
        let async_cm = self
            .inner
            .complete_accept(conn_id, &ConnParam::default())
            .await?;

        // Extract QP pointer AFTER the await (conn_id moved into async_cm)
        let qp = async_cm.qp_raw();

        // Create async CQ AFTER connection is established
        let notifier = TokioCqNotifier::new(comp_ch.fd()).map_err(crate::Error::Verbs)?;
        let async_cq = AsyncCq::new(cq, comp_ch, Box::new(notifier));
        let aqp = unsafe { AsyncQp::new(qp, async_cq) };

        let (event_channel, cm_id) = async_cm.into_parts();

        Ok(AsyncRdmaStream {
            _event_channel: event_channel,
            cm_id,
            _pd: pd,
            aqp,
            send_mr,
            recv_mrs,
            recv_pending: None,
            send_wr_seq: SEND_WR_ID,
            cq_state: CqPollState::default(),
            stashed_recv: VecDeque::new(),
            stashed_send: VecDeque::new(),
            write_pending: None,
        })
    }
}

// --- Helpers ---

fn stream_qp_attr() -> QpInitAttr {
    QpInitAttr {
        qp_type: QpType::Rc,
        max_send_wr: 16,
        max_recv_wr: 16,
        max_send_sge: 1,
        max_recv_sge: 1,
        max_inline_data: 0,
        sq_sig_all: true,
    }
}

/// Post a recv WR using raw ibverbs (doesn't go through AsyncQp since
/// we need to pre-post before the CQ is fully set up).
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
