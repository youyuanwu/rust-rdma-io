//! SendRecvTransport — concrete [`Transport`](crate::transport::Transport) using RDMA Send/Recv.
//!
//! Owns the QP, buffers, CQ state, and CM connection lifecycle. Created via
//! [`connect`](SendRecvTransport::connect) (client) or
//! [`accept`](SendRecvTransport::accept) (server).

use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;

use crate::async_cm::{AsyncCmId, AsyncCmListener};
use crate::async_cq::AsyncCq;
use crate::async_qp::AsyncQp;
use crate::cm::{CmId, ConnParam, EventChannel, PortSpace};
use crate::completion_source::CompletionSource;
use crate::mr::{AccessFlags, OwnedMemoryRegion};
use crate::pd::ProtectionDomain;
use crate::qp::QpInitAttr;
use crate::transport::{RecvCompletion, Transport, TransportBuilder};
use crate::wc::WorkCompletion;
use crate::wr::QpType;

/// Bounded number of teardown drain sweeps in `Drop`. After forcing the QP to
/// ERROR the outstanding WRs flush locally (microseconds), so this is a short
/// spin, not a wall-clock wait; `ibv_destroy_qp` is the backstop if exhausted.
const SEND_RECV_TEARDOWN_DRAIN_POLLS: usize = 4096;

/// Configuration for creating an [`SendRecvTransport`].
#[derive(Debug, Clone)]
pub struct SendRecvConfig {
    /// Size of each buffer (send and recv).
    pub buf_size: usize,
    /// Number of pre-posted recv buffers.
    pub num_recv_bufs: usize,
    /// Number of send buffers.
    pub num_send_bufs: usize,
    /// Max inline data size (0 = disabled).
    pub max_inline_data: u32,
    /// QP type (RC for both initially).
    pub qp_type: QpType,
}

impl Default for SendRecvConfig {
    fn default() -> Self {
        Self::stream()
    }
}

impl SendRecvConfig {
    /// Configuration tuned for byte stream workloads (gRPC/tonic).
    pub fn stream() -> Self {
        Self {
            buf_size: 64 * 1024,
            num_recv_bufs: 8,
            num_send_bufs: 1,
            max_inline_data: 0,
            qp_type: QpType::Rc,
        }
    }

    /// Stream configuration sized to keep `depth` sends outstanding.
    ///
    /// [`stream`](Self::stream) posts one signaled send at a time; that caps a
    /// byte stream (and therefore gRPC/HTTP-2 over it) to a single in-flight
    /// RDMA message per connection. Sizing `num_send_bufs` (and matching recv
    /// buffers) to `depth` lets [`AsyncRdmaStream`](crate::async_stream::AsyncRdmaStream)
    /// pipeline up to `depth` posted sends before it must reap a completion —
    /// the mechanism the echo benchmark uses to raise throughput. Both peers
    /// must be sized for the depth they intend to pipeline.
    pub fn stream_with_depth(depth: usize) -> Self {
        let depth = depth.max(1);
        Self {
            buf_size: 64 * 1024,
            num_recv_bufs: (depth + 2).max(8),
            num_send_bufs: (depth + 1).max(2),
            max_inline_data: 0,
            qp_type: QpType::Rc,
        }
    }

    /// Configuration tuned for datagram workloads (Quinn/QUIC).
    pub fn datagram() -> Self {
        Self {
            buf_size: 1500,
            num_recv_bufs: 64,
            num_send_bufs: 4,
            max_inline_data: 64,
            qp_type: QpType::Rc,
        }
    }
}

/// RDMA transport using Send/Recv (two-sided) verbs.
///
/// **Drop order is critical.** Fields drop in declaration order:
/// 1. State fields (no RDMA teardown)
/// 2. QP (destroys QP, flushes all outstanding WRs)
/// 3. Completion sources (destroy the send/recv CQs — after the QP)
/// 4. MRs (safe to deregister after QP destroy flushed all WRs)
/// 5. PD (ref-counted, safe anytime)
/// 6. cm_async_fd (deregister from epoll BEFORE closing the fd)
/// 7. cm_id (disconnect/destroy)
/// 8. event_channel (closes the fd — must be LAST)
pub struct SendRecvTransport {
    // -- State (no RDMA teardown) --
    disconnected: bool,
    peer_disconnected: bool,
    next_send_idx: usize,
    send_in_flight: Vec<bool>,
    config: SendRecvConfig,

    // -- RDMA data-path resources (drop: QP → CQs → MRs → PD) --
    qp: AsyncQp,
    // Sources own the send/recv CQs; declared AFTER `qp` so RAII destroys the
    // QP before its CQs (the verbs "destroy QP before its CQ" rule).
    send_src: CompletionSource,
    recv_src: CompletionSource,
    send_bufs: Vec<OwnedMemoryRegion>,
    recv_bufs: Vec<OwnedMemoryRegion>,
    _pd: Arc<ProtectionDomain>,

    // -- CM resources (drop: AsyncFd → CmId → EventChannel) --
    cm_async_fd: AsyncFd<RawFd>,
    cm_id: CmId,
    event_channel: EventChannel,
}

impl SendRecvTransport {
    /// Connect to a remote RDMA endpoint (client side).
    pub async fn connect(addr: &SocketAddr, config: SendRecvConfig) -> crate::Result<Self> {
        let async_cm = AsyncCmId::new(PortSpace::Tcp)?;
        async_cm.resolve_addr(None, addr, 2000).await?;
        async_cm.resolve_route(2000).await?;

        let ctx = async_cm
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = async_cm.alloc_pd()?;

        let send_cq_depth = config.num_send_bufs as i32 + 1;
        let recv_cq_depth = config.num_recv_bufs as i32 + 1;
        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = make_qp_attr(&config);
        let cmqp =
            async_cm.create_qp_with_cq(&pd, &qp_attr, Some(send_cq.cq()), Some(recv_cq.cq()))?;

        let (send_bufs, recv_bufs) = alloc_buffers(&pd, &config)?;
        let qp = AsyncQp::new_poster(cmqp);
        let send_src = CompletionSource::arm_park(send_cq);
        let recv_src = CompletionSource::arm_park(recv_cq);

        for (i, mr) in recv_bufs.iter().enumerate() {
            qp.post_recv_buffer(mr, i as u64)?;
        }

        async_cm.connect(&ConnParam::default()).await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        Ok(Self::from_parts(
            qp,
            send_src,
            recv_src,
            cm_async_fd,
            cm_id,
            event_channel,
            pd,
            send_bufs,
            recv_bufs,
            config,
        ))
    }

    /// Accept a connection from a listener (server side).
    pub async fn accept(listener: &AsyncCmListener, config: SendRecvConfig) -> crate::Result<Self> {
        let conn_id = listener.get_request().await?;
        Self::complete_accept(conn_id, listener, config).await
    }

    /// Complete accept using a pre-obtained `CmId` from `poll_get_request`.
    ///
    /// Sets up QP, buffers, runs the accept handshake, and migrates the
    /// connection to its own event channel.
    pub async fn complete_accept(
        conn_id: crate::cm::CmId,
        listener: &AsyncCmListener,
        config: SendRecvConfig,
    ) -> crate::Result<Self> {
        let ctx = conn_id
            .verbs_context()
            .ok_or(crate::Error::InvalidArg("no verbs context".into()))?;
        let pd = conn_id.alloc_pd()?;

        let send_cq_depth = config.num_send_bufs as i32 + 1;
        let recv_cq_depth = config.num_recv_bufs as i32 + 1;
        let send_cq = AsyncCq::create_tokio(ctx.clone(), send_cq_depth)?;
        let recv_cq = AsyncCq::create_tokio(ctx, recv_cq_depth)?;

        let qp_attr = make_qp_attr(&config);
        let cmqp =
            conn_id.create_qp_with_cq(&pd, &qp_attr, Some(send_cq.cq()), Some(recv_cq.cq()))?;

        let (send_bufs, recv_bufs) = alloc_buffers(&pd, &config)?;
        let qp = AsyncQp::new_poster(cmqp);
        let send_src = CompletionSource::arm_park(send_cq);
        let recv_src = CompletionSource::arm_park(recv_cq);

        for (i, mr) in recv_bufs.iter().enumerate() {
            qp.post_recv_buffer(mr, i as u64)?;
        }

        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await?;

        let (event_channel, cm_id) = async_cm.into_parts();
        let cm_async_fd = AsyncFd::new(event_channel.fd()).map_err(crate::Error::Verbs)?;

        Ok(Self::from_parts(
            qp,
            send_src,
            recv_src,
            cm_async_fd,
            cm_id,
            event_channel,
            pd,
            send_bufs,
            recv_bufs,
            config,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        qp: AsyncQp,
        send_src: CompletionSource,
        recv_src: CompletionSource,
        cm_async_fd: AsyncFd<RawFd>,
        cm_id: CmId,
        event_channel: EventChannel,
        pd: Arc<ProtectionDomain>,
        send_bufs: Vec<OwnedMemoryRegion>,
        recv_bufs: Vec<OwnedMemoryRegion>,
        config: SendRecvConfig,
    ) -> Self {
        let num_send = config.num_send_bufs;
        Self {
            disconnected: false,
            peer_disconnected: false,
            next_send_idx: 0,
            send_in_flight: vec![false; num_send],
            config,
            qp,
            send_src,
            recv_src,
            send_bufs,
            recv_bufs,
            _pd: pd,
            cm_async_fd,
            cm_id,
            event_channel,
        }
    }

    fn check_cm_event(&mut self) -> bool {
        match self.event_channel.try_get_event() {
            Ok(ev) => {
                let etype = ev.event_type();
                ev.ack();
                if etype == crate::cm::CmEventType::Disconnected {
                    self.peer_disconnected = true;
                }
                self.peer_disconnected
            }
            Err(crate::Error::WouldBlock) => false,
            Err(_) => {
                self.peer_disconnected = true;
                true
            }
        }
    }
}

impl Transport for SendRecvTransport {
    fn send_copy(&mut self, data: &[u8]) -> crate::Result<usize> {
        // Find a free send buffer (round-robin with in-flight check)
        let n = self.send_bufs.len();
        let start = self.next_send_idx % n;
        let mut idx = start;
        loop {
            if !self.send_in_flight[idx] {
                break;
            }
            idx = (idx + 1) % n;
            if idx == start {
                return Ok(0); // All send buffers occupied
            }
        }

        let mr = &mut self.send_bufs[idx];
        let len = data.len().min(mr.as_slice().len());
        mr.as_mut_slice()[..len].copy_from_slice(&data[..len]);
        let wr_id = self.config.num_recv_bufs as u64 + idx as u64;
        self.qp.post_send_signaled(mr, 0, len, wr_id)?;
        self.send_in_flight[idx] = true;
        self.next_send_idx = (idx + 1) % n;
        Ok(len)
    }

    fn send_gather(&mut self, bufs: &[std::io::IoSlice<'_>]) -> crate::Result<usize> {
        // Gather the caller's slices straight into a registered send buffer
        // (one copy) instead of coalescing into a scratch first (two copies).
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        if total == 0 {
            return Ok(0);
        }
        // Find a free send buffer (round-robin with in-flight check).
        let n = self.send_bufs.len();
        let start = self.next_send_idx % n;
        let mut idx = start;
        loop {
            if !self.send_in_flight[idx] {
                break;
            }
            idx = (idx + 1) % n;
            if idx == start {
                return Ok(0); // All send buffers occupied
            }
        }

        let wr_id = self.config.num_recv_bufs as u64 + idx as u64;
        let mr = &mut self.send_bufs[idx];
        let len = total.min(mr.as_slice().len());
        crate::transport_common::gather_into(&mut mr.as_mut_slice()[..len], bufs);
        self.qp.post_send_signaled(mr, 0, len, wr_id)?;
        self.send_in_flight[idx] = true;
        self.next_send_idx = (idx + 1) % n;
        Ok(len)
    }

    fn poll_send_completion(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<()>> {
        let mut wc_buf = [WorkCompletion::default(); 4];
        let n = match self.send_src.poll_completions(cx, &mut wc_buf) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => n,
        };
        for wc in &wc_buf[..n] {
            if !wc.is_success() {
                self.peer_disconnected = true;
                return Poll::Ready(Err(crate::Error::WorkCompletion {
                    status: wc.status_raw(),
                    vendor_err: wc.vendor_err(),
                }));
            }
            let wr_id = wc.wr_id() as usize;
            if let Some(buf_idx) = wr_id
                .checked_sub(self.config.num_recv_bufs)
                .filter(|&idx| idx < self.send_in_flight.len())
            {
                self.send_in_flight[buf_idx] = false;
            }
        }
        Poll::Ready(Ok(()))
    }

    fn sends_in_flight(&self) -> usize {
        self.send_in_flight.iter().filter(|&&busy| busy).count()
    }

    fn recv_window(&self) -> usize {
        self.recv_bufs.len()
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [RecvCompletion],
    ) -> Poll<crate::Result<usize>> {
        let max = out.len().min(8);
        let mut wc_buf = [WorkCompletion::default(); 8];
        let n = match self.recv_src.poll_completions(cx, &mut wc_buf[..max]) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => n,
        };
        for i in 0..n {
            if !wc_buf[i].is_success() {
                self.peer_disconnected = true;
                return Poll::Ready(Err(crate::Error::WorkCompletion {
                    status: wc_buf[i].status_raw(),
                    vendor_err: wc_buf[i].vendor_err(),
                }));
            }
            out[i] = RecvCompletion {
                buf_idx: wc_buf[i].wr_id() as usize,
                byte_len: (wc_buf[i].byte_len() as usize).min(self.config.buf_size),
            };
        }
        Poll::Ready(Ok(n))
    }

    fn recv_buf(&self, buf_idx: usize) -> &[u8] {
        self.recv_bufs[buf_idx].as_slice()
    }

    fn repost_recv(&mut self, buf_idx: usize) -> crate::Result<()> {
        self.qp
            .post_recv_buffer(&self.recv_bufs[buf_idx], buf_idx as u64)
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> bool {
        if self.peer_disconnected {
            return true;
        }
        loop {
            match self.cm_async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(mut guard)) => {
                    guard.clear_ready();
                    if self.check_cm_event() {
                        return true;
                    }
                }
                Poll::Pending => {
                    return false;
                }
                Poll::Ready(Err(_)) => {
                    self.peer_disconnected = true;
                    return true;
                }
            }
        }
    }

    fn disconnect(&mut self) -> crate::Result<()> {
        if !self.disconnected {
            self.cm_id.disconnect()?;
            self.disconnected = true;
        }
        Ok(())
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.cm_id.local_addr()
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.cm_id.peer_addr()
    }
}

impl Drop for SendRecvTransport {
    fn drop(&mut self) {
        if !self.disconnected {
            let _ = self.cm_id.disconnect();
        }
        // Force the QP to ERROR so every outstanding WR flushes *locally* and
        // synchronously, then reap the resulting CQEs through the sources so the
        // QP releases its MR references before the MRs are deregistered (field
        // drop order: qp → sources → MRs). Forcing ERROR first avoids the race
        // where the async `cm_id.disconnect()` has not yet flushed the WRs, so
        // the old "poll once until empty" could under-drain. Bounded so `Drop`
        // never blocks the calling (possibly tokio worker) thread; the flush is
        // local (microseconds), and `ibv_destroy_qp` in the `qp` field drop is
        // the backstop.
        let _ = self.qp.to_error();
        let mut wc = [crate::wc::WorkCompletion::default(); 16];
        for _ in 0..SEND_RECV_TEARDOWN_DRAIN_POLLS {
            let s = self.send_src.try_drain(&mut wc).unwrap_or(0);
            let r = self.recv_src.try_drain(&mut wc).unwrap_or(0);
            if s == 0 && r == 0 {
                break;
            }
        }
    }
}

// --- Helpers ---

fn make_qp_attr(config: &SendRecvConfig) -> QpInitAttr {
    QpInitAttr {
        qp_type: config.qp_type,
        max_send_wr: config.num_send_bufs as u32 + 1,
        max_recv_wr: config.num_recv_bufs as u32 + 1,
        max_send_sge: 1,
        max_recv_sge: 1,
        max_inline_data: config.max_inline_data,
        sq_sig_all: true,
    }
}

fn alloc_buffers(
    pd: &Arc<ProtectionDomain>,
    config: &SendRecvConfig,
) -> crate::Result<(Vec<OwnedMemoryRegion>, Vec<OwnedMemoryRegion>)> {
    let access = AccessFlags::LOCAL_WRITE;
    let send_bufs = (0..config.num_send_bufs)
        .map(|_| pd.reg_mr_owned(vec![0u8; config.buf_size], access))
        .collect::<crate::Result<Vec<_>>>()?;
    let recv_bufs = (0..config.num_recv_bufs)
        .map(|_| pd.reg_mr_owned(vec![0u8; config.buf_size], access))
        .collect::<crate::Result<Vec<_>>>()?;
    Ok((send_bufs, recv_bufs))
}

impl TransportBuilder for SendRecvConfig {
    type Transport = SendRecvTransport;

    async fn connect(&self, addr: &SocketAddr) -> crate::Result<SendRecvTransport> {
        SendRecvTransport::connect(addr, self.clone()).await
    }

    async fn accept(&self, listener: &AsyncCmListener) -> crate::Result<SendRecvTransport> {
        SendRecvTransport::accept(listener, self.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::SendRecvConfig;

    // stream() posts one send at a time -> a single in-flight message.
    #[test]
    fn stream_has_single_send_buffer() {
        assert_eq!(SendRecvConfig::stream().num_send_bufs, 1);
    }

    // stream_with_depth sizes send/recv buffers to pipeline `depth` sends.
    #[test]
    fn stream_with_depth_sizes_buffers_for_pipeline() {
        let c = SendRecvConfig::stream_with_depth(16);
        assert_eq!(c.num_send_bufs, 17); // depth + 1
        assert_eq!(c.num_recv_bufs, 18); // depth + 2
        assert_eq!(c.buf_size, 64 * 1024);
    }

    // Buffer counts respect their floors and depth 0 clamps to 1.
    #[test]
    fn stream_with_depth_enforces_minimums() {
        let c = SendRecvConfig::stream_with_depth(0);
        assert_eq!(c.num_send_bufs, 2); // (1 + 1).max(2)
        assert_eq!(c.num_recv_bufs, 8); // (1 + 2).max(8)
    }
}
