//! RDMA Stream — `std::io::Read` + `std::io::Write` over RDMA SEND/RECV.
//!
//! Provides TCP-like semantics over rdma_cm connections.
//!
//! # Buffer Architecture
//!
//! Each stream has:
//! - **Send buffer**: Pre-registered MR for outgoing data. `write()` copies
//!   user data into it and posts a SEND WR, then waits for completion.
//! - **Recv buffers**: Two pre-registered MRs (double-buffered). Both are
//!   posted as RECV WRs so incoming data can land immediately.
//!
//! # Protocol
//!
//! Pure RDMA SEND/RECV with no application-level framing. Each `write()`
//! becomes one RDMA SEND; each `read()` consumes one RDMA RECV completion.
//! Messages are bounded by the buffer size (default 64 KiB).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use crate::cm::{CmEventType, CmId, ConnParam, EventChannel, PortSpace};
use crate::cq::CompletionQueue;
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;
use crate::wc::WorkCompletion;
use crate::wr::QpType;

/// Default buffer size for stream send/recv (64 KiB).
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// Number of recv buffers (double-buffered).
const NUM_RECV_BUFS: usize = 2;

/// WR ID for send operations.
const SEND_WR_ID: u64 = 100;

/// An RDMA stream implementing `std::io::Read` and `std::io::Write`.
///
/// Created via [`RdmaStream::connect`] (client) or [`RdmaListener::accept`]
/// (server). Internally manages registered buffers and CQ polling.
///
/// # Example
///
/// ```no_run
/// use rdma_io::stream::{RdmaListener, RdmaStream};
/// use std::io::{Read, Write};
///
/// // Server
/// let listener = RdmaListener::bind(&"0.0.0.0:9999".parse().unwrap()).unwrap();
/// let mut stream = listener.accept().unwrap();
/// let mut buf = [0u8; 1024];
/// let n = stream.read(&mut buf).unwrap();
///
/// // Client
/// let mut stream = RdmaStream::connect(&"10.0.0.1:9999".parse().unwrap()).unwrap();
/// stream.write_all(b"hello").unwrap();
/// ```
pub struct RdmaStream {
    _event_channel: EventChannel,
    cm_id: CmId,
    _pd: Arc<ProtectionDomain>,
    cq: Arc<CompletionQueue>,
    send_mr: OwnedMemoryRegion,
    recv_mrs: [OwnedMemoryRegion; NUM_RECV_BUFS],
    /// Buffered recv data: (buffer_index, offset, length).
    recv_pending: Option<(usize, usize, usize)>,
}

impl RdmaStream {
    /// Connect to a remote RDMA endpoint (client side).
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
        let cq = CompletionQueue::new(Arc::clone(&ctx), 32)?;

        cm_id.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        let send_mr = pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs = [
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
        ];

        let mut stream = RdmaStream {
            _event_channel: ch,
            cm_id,
            _pd: pd,
            cq,
            send_mr,
            recv_mrs,
            recv_pending: None,
        };

        stream.post_recv(0)?;
        stream.post_recv(1)?;

        stream.cm_id.connect(&ConnParam::default())?;
        expect_event(&stream._event_channel, CmEventType::Established)?.ack();

        Ok(stream)
    }

    /// Create a stream from an already-connected CM ID (used by RdmaListener).
    fn from_accepted(
        event_channel: EventChannel,
        cm_id: CmId,
        pd: Arc<ProtectionDomain>,
        cq: Arc<CompletionQueue>,
        buf_size: usize,
    ) -> crate::Result<Self> {
        let send_mr = pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?;
        let recv_mrs = [
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
            pd.reg_mr_owned(vec![0u8; buf_size], AccessFlags::LOCAL_WRITE)?,
        ];

        let mut stream = RdmaStream {
            _event_channel: event_channel,
            cm_id,
            _pd: pd,
            cq,
            send_mr,
            recv_mrs,
            recv_pending: None,
        };

        stream.post_recv(0)?;
        stream.post_recv(1)?;

        Ok(stream)
    }

    fn post_recv(&mut self, buf_idx: usize) -> crate::Result<()> {
        let mr = &self.recv_mrs[buf_idx];
        let mut sge = ibv_sge {
            addr: unsafe { (*mr.as_raw()).addr as u64 },
            length: mr.as_slice().len() as u32,
            lkey: mr.lkey(),
        };
        let mut wr = ibv_recv_wr {
            wr_id: buf_idx as u64,
            sg_list: &mut sge,
            num_sge: 1,
            ..Default::default()
        };
        let mut bad_wr: *mut ibv_recv_wr = std::ptr::null_mut();
        let qp = self.cm_id.qp_raw();
        let ret = unsafe { rdma_wrap_ibv_post_recv(qp, &mut wr, &mut bad_wr) };
        if ret != 0 {
            return Err(crate::Error::Verbs(io::Error::from_raw_os_error(-ret)));
        }
        Ok(())
    }

    /// Poll CQ until a completion with `expected_wr_id` arrives.
    /// Buffers unexpected recv completions.
    fn poll_completion(&mut self, expected_wr_id: u64) -> crate::Result<WorkCompletion> {
        let mut wc = [WorkCompletion::default(); 4];
        loop {
            let n = self.cq.poll(&mut wc)?;
            for wc_item in &wc[..n] {
                if !wc_item.is_success() {
                    return Err(crate::Error::WorkCompletion {
                        status: wc_item.status_raw(),
                        vendor_err: wc_item.vendor_err(),
                    });
                }
                if wc_item.wr_id() == expected_wr_id {
                    return Ok(*wc_item);
                }
                // Buffer unexpected recv completions
                let wr_id = wc_item.wr_id();
                if wr_id < NUM_RECV_BUFS as u64 {
                    self.recv_pending = Some((wr_id as usize, 0, wc_item.byte_len() as usize));
                }
            }
            std::thread::yield_now();
        }
    }

    /// Disconnect the stream gracefully.
    pub fn shutdown(&self) -> crate::Result<()> {
        self.cm_id.disconnect()
    }
}

impl io::Read for RdmaStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
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
                self.post_recv(buf_idx).map_err(io::Error::other)?;
            }
            return Ok(copy_len);
        }

        // Poll CQ for a recv completion
        let mut wc = [WorkCompletion::default(); 4];
        loop {
            let n = self.cq.poll(&mut wc).map_err(io::Error::other)?;
            for wc_item in &wc[..n] {
                if !wc_item.is_success() {
                    if wc_item.status_raw() == IBV_WC_WR_FLUSH_ERR {
                        return Ok(0); // EOF on disconnect
                    }
                    return Err(io::Error::other(format!(
                        "WC error: status={}",
                        wc_item.status_raw()
                    )));
                }
                let wr_id = wc_item.wr_id();
                if wr_id < NUM_RECV_BUFS as u64 {
                    let byte_len = wc_item.byte_len() as usize;
                    if byte_len == 0 {
                        return Ok(0);
                    }
                    let buf_idx = wr_id as usize;
                    let copy_len = byte_len.min(buf.len());
                    buf[..copy_len]
                        .copy_from_slice(&self.recv_mrs[buf_idx].as_slice()[..copy_len]);
                    if copy_len < byte_len {
                        self.recv_pending = Some((buf_idx, copy_len, byte_len));
                    } else {
                        self.post_recv(buf_idx).map_err(io::Error::other)?;
                    }
                    return Ok(copy_len);
                }
                // Ignore send completions during read
            }
            std::thread::yield_now();
        }
    }
}

impl io::Write for RdmaStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let send_len = buf.len().min(self.send_mr.as_slice().len());
        self.send_mr.as_mut_slice()[..send_len].copy_from_slice(&buf[..send_len]);

        let mut sge = ibv_sge {
            addr: unsafe { (*self.send_mr.as_raw()).addr as u64 },
            length: send_len as u32,
            lkey: self.send_mr.lkey(),
        };
        let mut wr = ibv_send_wr {
            wr_id: SEND_WR_ID,
            opcode: IBV_WR_SEND,
            send_flags: IBV_SEND_SIGNALED,
            sg_list: &mut sge,
            num_sge: 1,
            ..Default::default()
        };
        let mut bad_wr: *mut ibv_send_wr = std::ptr::null_mut();
        let qp = self.cm_id.qp_raw();
        let ret = unsafe { rdma_wrap_ibv_post_send(qp, &mut wr, &mut bad_wr) };
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(-ret));
        }

        let wc = self.poll_completion(SEND_WR_ID).map_err(io::Error::other)?;
        if !wc.is_success() {
            return Err(io::Error::other(format!(
                "send WC error: status={}",
                wc.status_raw()
            )));
        }

        Ok(send_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RdmaStream {
    fn drop(&mut self) {
        if let Err(e) = self.cm_id.disconnect() {
            tracing::error!("RdmaStream disconnect failed: {e}");
        }
    }
}

/// An RDMA listener, analogous to `TcpListener`.
///
/// Binds to a local address and accepts incoming RDMA connections,
/// returning an [`RdmaStream`] for each.
pub struct RdmaListener {
    event_channel: EventChannel,
    _cm_id: CmId,
    buf_size: usize,
}

impl RdmaListener {
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

    /// Accept an incoming connection, returning an [`RdmaStream`].
    ///
    /// Blocks until a client connects.
    pub fn accept(&self) -> crate::Result<RdmaStream> {
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
        let cq = CompletionQueue::new(Arc::clone(&ctx), 32)?;
        let conn_ch = EventChannel::new()?;

        conn_id.create_qp_with_cq(&pd, &stream_qp_attr(), Some(&cq), Some(&cq))?;

        let stream =
            RdmaStream::from_accepted(conn_ch, conn_id, pd, cq, self.buf_size)?;

        stream.cm_id.accept(&ConnParam::default())?;
        expect_event(&self.event_channel, CmEventType::Established)?.ack();

        Ok(stream)
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
