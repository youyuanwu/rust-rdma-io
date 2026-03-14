# AsyncRdmaStream Improvement Opportunities

**Status:** Design  
**Date:** 2026-03-14  
**Context:** Learnings from rsocket internal analysis ([Rsocket.md](../background/Rsocket.md))

## Current Design Summary

Our `AsyncRdmaStream` uses **Send/Recv** verbs over an RC QP with:

- **Dual CQ** — separate send_cq and recv_cq (no lock contention)
- **8 pre-posted recv buffers** × 64 KiB each
- **1 send buffer** × 64 KiB, gated by `write_pending` (1 in-flight max)
- **Implicit flow control** — RNR retry (count=7, infinite) + recv buffer count
- **No inline data** (`max_inline_data: 0`)
- **No busy-poll** — direct arm → block via `AsyncFd`
- **tokio-native** — `AsyncFd` on comp_channel fds, drain-after-arm pattern

Total memory per connection: **576 KiB** (9 × 64 KiB registered buffers).

## rsocket Comparison

rsocket implements a byte stream using **RDMA Write + Immediate Data** with ring buffers and credit-based flow control. Key differences:

| Aspect | Our AsyncRdmaStream | rsocket |
|--------|---------------------|---------|
| **Verb** | Send/Recv (two-sided) | RDMA Write+Imm (one-sided) |
| **CQ** | Dual (no contention) | Single shared (2 locks) |
| **Flow control** | Implicit (8 recv bufs + RNR) | Explicit credits (seq nums + SGL) |
| **Send depth** | 1 in-flight | Up to 384, pipelined |
| **Buffers** | 9 × 64 KiB = 576 KiB | 128 KiB send ring + 128 KiB recv ring = 256 KiB |
| **Inline data** | Disabled | 64 bytes default |
| **Busy-poll** | None | 10 µs before blocking |
| **Send sizing** | Full buffer each time | 2 KiB → 64 KiB exponential ramp |
| **iWARP** | Works as-is | Needs 2 WRs/msg fallback (Write + inline Send) |

### What's Already Better in Our Design

1. **Dual CQ** — poll_read and poll_write are truly independent; rsocket's single CQ requires `cq_lock` + `cq_wait_lock` serialization
2. **Simpler flow control** — RNR retry handles backpressure at the HCA level; no credit protocol, no SGL exchange, no sequence numbers
3. **tokio-native** — direct `AsyncFd` integration; rsocket needs a dedicated `rpoll` reactor thread for async usage
4. **iWARP-portable** — Send/Recv works identically on InfiniBand, RoCE, and iWARP; rsocket must detect iWARP and switch to Write + inline Send (doubling SQ consumption)
5. **Robust disconnect** — CM event channel + QP state fallback catches disconnects on all providers (including rxe where CQ flush is unreliable)

## Proposed Improvements

### 1. Enable Inline Data for Small Messages

**Effort:** Small | **Impact:** Latency for small messages

**Problem:** `max_inline_data` is set to 0. Every send — even a 16-byte gRPC header — requires the HCA to DMA-read the data from the MR via PCIe. For small messages, this DMA round-trip dominates latency.

**Solution:** Enable 64-byte inline. When the send payload fits within the inline threshold, the data is copied directly into the WQE — the HCA reads it from the WQE itself, eliminating one PCIe DMA round-trip.

```rust
// stream_qp_attr() change:
QpInitAttr {
    max_inline_data: 64,  // was 0
    ..
}

// post_send_signaled() change:
let flags = if length <= self.sq_inline_threshold {
    SendFlags::SIGNALED | SendFlags::INLINE
} else {
    SendFlags::SIGNALED
};
```

**Why 64 bytes:** rsocket's default. Covers HTTP/2 SETTINGS, WINDOW_UPDATE, PING, small gRPC metadata frames. HCA WQE size increases slightly but the latency win on small messages outweighs it.

**What changes:** QP creation requests higher inline data; HCA may allocate slightly larger WQEs. No change to recv path.

### 2. Double-Buffered Sends

**Effort:** Medium | **Impact:** Throughput for bulk/streaming data only

**Problem:** `write_pending` gates poll_write to exactly 1 send in-flight. The send path is:

```
[copy → post → WAIT → complete] [copy → post → WAIT → complete] ...
                 ↑ idle while HCA processes
```

The HCA sits idle during the copy phase of the next message. For streaming workloads (gRPC server streaming, file transfer), this serialization halves potential throughput.

**Solution:** Two send buffers, alternating. Post the next send while the HCA processes the previous one:

```
Buffer A: [copy → post]                    [copy → post]
Buffer B:               [copy → post]                    [copy → post]
HCA:      [----------process A---][---process B---][---process A---]
```

```rust
struct AsyncRdmaStream {
    send_mrs: [OwnedMemoryRegion; 2],  // was: send_mr (single)
    send_slot: usize,                   // alternates 0/1
    write_pending: [Option<usize>; 2],  // per-slot pending state
    ..
}
```

**poll_write logic:**
1. Find a free send slot (check if `write_pending[send_slot]` is None)
2. If no free slot, poll send_cq to complete the oldest pending send
3. Copy data into `send_mrs[send_slot]`, post send
4. Advance `send_slot` to the other buffer
5. Return `Ok(len)` immediately (send is in-flight, completion checked next call)

**QP change:** `max_send_wr` stays at 2 (already sufficient). Send CQ depth stays at 2.

**Tradeoff:** Slightly more complex poll_write state machine. Memory increases by 64 KiB per connection (1 extra send MR).

#### Ordering Guarantee

**RC QPs guarantee strict FIFO ordering.** The SQ is a hardware FIFO — the HCA transmits WQEs in posting order, and the peer's recv completions arrive in that same order. Even with multiple outstanding sends, message N always arrives before message N+1. There is no risk of out-of-order delivery.

#### AsyncWrite Contract and poll_flush Impact

Returning `Ok(n)` from `poll_write` before the send completes is valid per the `AsyncWrite` contract — it means "n bytes accepted," not "delivered to peer." This matches TCP semantics (kernel buffer acceptance, not ACK). The user data is already copied into the send MR, so the user's buffer is safe to reuse.

However, `poll_flush` **can no longer be a no-op.** It must drain the send CQ to ensure all in-flight sends have completed:

```rust
fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    // Must wait for all pending sends to complete
    for slot in 0..2 {
        if self.write_pending[slot].is_some() {
            // poll send_cq until this slot's completion arrives
            // ...
        }
    }
    Poll::Ready(Ok(()))
}
```

`poll_close` must similarly drain both slots before sending DREQ.

#### gRPC Relevance

**Marginal benefit for gRPC.** Typical unary RPCs produce 1-2 small `poll_write` calls (HEADERS + DATA frame). The h2 codec buffers internally and the bottleneck is serialization and HTTP/2 framing, not RDMA send latency. Double-buffering only helps for sustained streaming workloads (server/client streaming with large payloads) where many consecutive writes occur back-to-back.

### 3. Configurable Busy-Poll

**Effort:** Small | **Impact:** Latency under sustained load

**Problem:** Our drain-after-arm pattern always arms CQ notifications and blocks on the comp_channel fd. This involves: `ibv_req_notify_cq` → kernel `poll()` → `ibv_get_cq_event` → `ibv_ack_cq_events`. For back-to-back messages, the completion is often already in the CQ by the time we check — the arm/block cycle adds ~5-10 µs of unnecessary overhead.

**Solution:** Add an optional busy-poll phase before arming, similar to rsocket's `polling_time`:

```rust
pub fn poll_completions(
    &self,
    cx: &mut Context<'_>,
    state: &mut CqPollState,
    wc_buf: &mut [WorkCompletion],
) -> Poll<Result<usize>> {
    // Phase 1: Quick poll (no arm overhead)
    if let Some(duration) = self.busy_poll_duration {
        let start = Instant::now();
        while start.elapsed() < duration {
            let n = self.cq.poll(wc_buf)?;
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
            std::hint::spin_loop();
        }
    }

    // Phase 2: Existing drain-after-arm pattern
    // ...
}
```

**Configuration:** Default off (0 µs). Enable via builder option:

```rust
AsyncRdmaStream::builder()
    .busy_poll(Duration::from_micros(10))
    .connect(addr)
    .await?
```

**Tradeoff:** Burns CPU during the busy-poll window. Only beneficial under sustained load where completions arrive within the poll window. Should be opt-in and documented.

**rsocket's approach:** 10 µs busy-poll (`polling_time` config, read from `/etc/rdma/rsocket/polling_time`). This is a reasonable default for latency-sensitive workloads.

### 4. Adaptive Send Sizing

**Effort:** Small | **Impact:** First-byte latency for interactive workloads

**Problem:** poll_write copies up to 64 KiB in a single send. For interactive protocols (gRPC unary RPCs), the first message is often small — but if the caller provides a large buffer, we copy and send all of it before returning. This delays the first response.

**Solution:** Start with a small transfer size and increase exponentially, matching rsocket's overlapped send strategy:

```rust
const INITIAL_SEND_SIZE: usize = 2048;
const MAX_SEND_SIZE: usize = 65536;

struct AsyncRdmaStream {
    next_send_size: usize,  // starts at INITIAL_SEND_SIZE
    ..
}

// In poll_write:
let send_len = buf.len()
    .min(self.send_mr.as_slice().len())
    .min(self.next_send_size);

// After successful send:
if self.next_send_size < MAX_SEND_SIZE {
    self.next_send_size = (self.next_send_size * 2).min(MAX_SEND_SIZE);
}
```

**Why it helps:** The first 2 KiB of a gRPC response arrives sooner. Subsequent chunks grow to fill the pipe. For tonic with typical <1 KiB unary responses, this has no effect (message already fits in one send). For streaming large payloads, convergence to 64 KiB takes only 5 doublings.

**Reset:** Reset `next_send_size` to `INITIAL_SEND_SIZE` after an idle period (no send for >1 ms) or on connection reuse.

## Priority

| # | Improvement | Effort | Impact | Priority |
|---|-------------|--------|--------|----------|
| 1 | Inline data (64 B) | Small | Latency for small msgs | **High** — nearly free, always beneficial |
| 2 | Double-buffered sends | Medium | Throughput for streaming only | **Low** — marginal for gRPC unary; adds poll_flush complexity |
| 3 | Busy-poll option | Small | Latency under load | **Medium** — opt-in, niche benefit |
| 4 | Adaptive send sizing | Small | First-byte latency | **Low** — nice-to-have, most gRPC msgs are small |

### Not Recommended

| Idea | Why Not |
|------|---------|
| **Ring buffer (rsocket-style)** | Adds pointer management, SGL exchange, credit protocol — significant complexity for modest memory savings (576 → 256 KiB). Our discrete buffers are simpler and the memory cost is acceptable. |
| **RDMA Write + Immediate Data** | Broken on both siw and rxe (see [Rsocket.md](../background/Rsocket.md)). Requires iWARP fallback (2 WRs/msg). Incompatible with our CI testing. Send/Recv is universally supported. |
| **Single shared CQ** | rsocket's approach requires two locks and careful serialization. Our dual CQ is simpler and avoids read/write contention entirely. |

## References

- [rsocket internals](../background/Rsocket.md#internal-implementation-analysis) — Full RDMA API analysis
- [async_stream.rs](../../rdma-io/src/async_stream.rs) — Current implementation
- [async_cq.rs](../../rdma-io/src/async_cq.rs) — CQ polling / drain-after-arm pattern
