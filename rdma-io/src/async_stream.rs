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

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::async_cq::AsyncCq;
use crate::async_qp::AsyncQp;
use crate::cm::{CmEventType, CmId, ConnParam, EventChannel, PortSpace};
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
/// let mut stream = listener.accept()?;
/// let mut buf = [0u8; 1024];
/// let n = stream.read(&mut buf).await?;
///
/// // Client
/// let mut stream = AsyncRdmaStream::connect(&"10.0.0.1:9999".parse().unwrap())?;
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
}

// Safety: All interior types are Send-safe. The CmId/AsyncQp raw pointers
// are guarded by libibverbs internal locking.
unsafe impl Send for AsyncRdmaStream {}

impl AsyncRdmaStream {
    /// Connect to a remote RDMA endpoint (client side).
    ///
    /// Connection setup is synchronous (uses rdma_cm). Once connected,
    /// all data transfer is async via `AsyncQp`.
    pub fn connect(addr: &SocketAddr) -> crate::Result<Self> {
        Self::connect_with_buf_size(addr, DEFAULT_BUF_SIZE)
    }

    /// Connect with a custom buffer size.
    pub fn connect_with_buf_size(addr: &SocketAddr, buf_size: usize) -> crate::Result<Self> {
        let ch = EventChannel::new()?;
        let cm_id = CmId::new(&ch, PortSpace::Tcp)?;

        cm_id.resolve_addr(None, addr, 2000)?;
        expect_event(&ch, CmEventType::AddrResolved)?.ack();

        cm_id.resolve_route(2000)?;
        expect_event(&ch, CmEventType::RouteResolved)?.ack();

        let ctx = cm_id
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = cm_id.alloc_pd()?;
        let comp_ch = CompletionChannel::new(&ctx)?;
        let cq = CompletionQueue::with_comp_channel(ctx, 32, &comp_ch)?;

        cm_id.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs = [
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
        ];

        // Pre-post recv buffers
        for (i, mr) in recv_mrs.iter().enumerate() {
            post_recv_raw(cm_id.qp_raw(), mr, i as u64)?;
        }

        cm_id.connect(&ConnParam::default())?;
        expect_event(&ch, CmEventType::Established)?.ack();

        // Create async CQ AFTER connection is established (AsyncFd registration
        // must happen inside a tokio runtime context).
        let notifier = TokioCqNotifier::new(comp_ch.fd()).map_err(crate::Error::Verbs)?;
        let async_cq = AsyncCq::new(cq, comp_ch, Box::new(notifier));
        let aqp = unsafe { AsyncQp::new(cm_id.qp_raw(), async_cq) };

        Ok(Self {
            _event_channel: ch,
            cm_id,
            _pd: pd,
            aqp,
            send_mr,
            recv_mrs,
            recv_pending: None,
            send_wr_seq: SEND_WR_ID,
        })
    }

    /// Read data from the stream asynchronously.
    ///
    /// Returns the number of bytes read. Returns `Ok(0)` on disconnect (EOF).
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Return buffered recv data first
        if let Some((buf_idx, offset, total_len)) = self.recv_pending {
            let remaining = total_len - offset;
            let copy_len = remaining.min(buf.len());
            buf[..copy_len]
                .copy_from_slice(&self.recv_mrs[buf_idx].as_slice()[offset..offset + copy_len]);

            if copy_len < remaining {
                self.recv_pending = Some((buf_idx, offset + copy_len, total_len));
            } else {
                self.recv_pending = None;
                post_recv_raw(self.cm_id.qp_raw(), &self.recv_mrs[buf_idx], buf_idx as u64)
                    .map_err(io::Error::other)?;
            }
            return Ok(copy_len);
        }

        // Poll CQ for a recv completion
        let mut wc = [WorkCompletion::default(); 4];
        loop {
            let n = self.aqp.poll(&mut wc).await.map_err(io::Error::other)?;
            for wc_item in &wc[..n] {
                if !wc_item.is_success() {
                    if wc_item.status_raw() == rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR {
                        return Ok(0); // EOF on disconnect
                    }
                    return Err(io::Error::other(format!(
                        "WC error: status={:?}",
                        wc_item.status()
                    )));
                }
                let wr_id = wc_item.wr_id();
                if wr_id < NUM_RECV_BUFS as u64 {
                    let byte_len = wc_item.byte_len() as usize;
                    if byte_len == 0 {
                        return Ok(0);
                    }
                    let bidx = wr_id as usize;
                    let copy_len = byte_len.min(buf.len());
                    buf[..copy_len].copy_from_slice(&self.recv_mrs[bidx].as_slice()[..copy_len]);
                    if copy_len < byte_len {
                        self.recv_pending = Some((bidx, copy_len, byte_len));
                    } else {
                        post_recv_raw(self.cm_id.qp_raw(), &self.recv_mrs[bidx], wr_id)
                            .map_err(io::Error::other)?;
                    }
                    return Ok(copy_len);
                }
                // Ignore send completions during read
            }
        }
    }

    /// Write data to the stream asynchronously.
    ///
    /// Returns the number of bytes written (bounded by buffer size).
    pub async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let send_len = data.len().min(self.send_mr.as_slice().len());
        self.send_mr.as_mut_slice()[..send_len].copy_from_slice(&data[..send_len]);

        let wr_id = self.send_wr_seq;
        self.send_wr_seq += 1;

        let wc = self
            .aqp
            .send(&self.send_mr, 0, send_len, wr_id)
            .await
            .map_err(io::Error::other)?;
        if !wc.is_success() {
            return Err(io::Error::other(format!(
                "send WC error: status={:?}",
                wc.status()
            )));
        }

        Ok(send_len)
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

/// An async RDMA listener, analogous to `TcpListener`.
///
/// Binds to a local address and accepts incoming RDMA connections,
/// returning an [`AsyncRdmaStream`] for each.
pub struct AsyncRdmaListener {
    event_channel: EventChannel,
    _cm_id: CmId,
    buf_size: usize,
}

impl AsyncRdmaListener {
    /// Bind to a local address and start listening.
    pub fn bind(addr: &SocketAddr) -> crate::Result<Self> {
        Self::bind_with_buf_size(addr, DEFAULT_BUF_SIZE)
    }

    /// Bind with a custom buffer size for accepted streams.
    pub fn bind_with_buf_size(addr: &SocketAddr, buf_size: usize) -> crate::Result<Self> {
        let ch = EventChannel::new()?;
        let cm_id = CmId::new(&ch, PortSpace::Tcp)?;
        cm_id.listen(addr, 128)?;
        Ok(Self {
            event_channel: ch,
            _cm_id: cm_id,
            buf_size,
        })
    }

    /// Accept an incoming connection, returning an [`AsyncRdmaStream`].
    ///
    /// Connection setup (CM handshake) is synchronous. Use
    /// `tokio::task::spawn_blocking` if you need to accept from an async context
    /// without blocking the executor.
    pub fn accept(&self) -> crate::Result<AsyncRdmaStream> {
        let ev = self.event_channel.get_event()?;
        let etype = ev.event_type();
        if etype != CmEventType::ConnectRequest {
            ev.ack();
            return Err(crate::Error::InvalidArg(format!(
                "expected ConnectRequest, got {etype:?}"
            )));
        }
        let conn_id = unsafe { CmId::from_raw(ev.cm_id_raw(), true) };
        ev.ack();

        let ctx = conn_id
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = conn_id.alloc_pd()?;
        let comp_ch = CompletionChannel::new(&ctx)?;
        let cq = CompletionQueue::with_comp_channel(ctx, 32, &comp_ch)?;
        let conn_ch = EventChannel::new()?;

        conn_id.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        // Post recv buffers and complete handshake BEFORE creating async resources
        let send_mr = pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs = [
            pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?,
            pd.reg_mr_owned(vec![0u8; self.buf_size], AccessFlags::LOCAL_WRITE)?,
        ];
        for (i, mr) in recv_mrs.iter().enumerate() {
            post_recv_raw(conn_id.qp_raw(), mr, i as u64)?;
        }

        conn_id.accept(&ConnParam::default())?;
        expect_event(&self.event_channel, CmEventType::Established)?.ack();

        // Migrate the accepted connection to its own event channel so the
        // listener can be safely dropped without affecting this connection.
        conn_id.migrate(&conn_ch)?;

        // Create async CQ AFTER connection is established
        let notifier = TokioCqNotifier::new(comp_ch.fd()).map_err(crate::Error::Verbs)?;
        let async_cq = AsyncCq::new(cq, comp_ch, Box::new(notifier));
        let aqp = unsafe { AsyncQp::new(conn_id.qp_raw(), async_cq) };

        Ok(AsyncRdmaStream {
            _event_channel: conn_ch,
            cm_id: conn_id,
            _pd: pd,
            aqp,
            send_mr,
            recv_mrs,
            recv_pending: None,
            send_wr_seq: SEND_WR_ID,
        })
    }
}

// --- Helpers ---

fn expect_event(ch: &EventChannel, expected: CmEventType) -> crate::Result<crate::cm::CmEvent> {
    let ev = ch.get_event()?;
    let actual = ev.event_type();
    if actual != expected {
        ev.ack();
        return Err(crate::Error::InvalidArg(format!(
            "expected {expected:?}, got {actual:?}"
        )));
    }
    Ok(ev)
}

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
