# Data-Path Copies: Transport & Stream Interfaces

An audit of every memory copy on the send and receive paths of the
[`Transport`](../../rdma-io/src/transport.rs) trait and the
[`AsyncRdmaStream`](../../rdma-io/src/async_stream.rs) adapter, which paths are
fundamental vs. reducible, and where the `bytes` crate's `Buf`/`BufMut`/`Bytes`
types would (and would not) help.

## Summary

The steady state is **one copy in each direction**. Two paths do two copies —
the vectored-write gather and the write-blocked receive stash. The single
copies are fundamental to bridging RDMA's registered-memory requirement to the
unregistered `&[u8]`/`&mut [u8]` of the `AsyncRead`/`AsyncWrite` traits; the
double copies are reducible.

**Perf context first:** A/B measurements on the MANA VMs found the vectored +
native-tokio work **perf-neutral** — rh2 is RTT/latency-bound, and the raw echo
path tops out around 6.7M msg/s pinned at ~9 cores on **completion/doorbell
overhead**, not memcpy. At today's operating points copies are *not* the
bottleneck. Copy reduction is a CPU-efficiency play for very high message rates,
not a throughput fix. See
[TonicRdmaVsTcpPerformance.md](TonicRdmaVsTcpPerformance.md) and
[EchoBenchmark.md](EchoBenchmark.md).

## Copy inventory

| Path | Copies | Where |
|---|---|---|
| **write** (single slice) | **1** | `send_copy`: user `&[u8]` → registered send MR ([send_recv_transport.rs](../../rdma-io/src/send_recv_transport.rs) `mr.as_mut_slice()[..len].copy_from_slice`; rings `.copy_from_slice` in [read_ring_transport.rs](../../rdma-io/src/read_ring_transport.rs) / [credit_ring_transport.rs](../../rdma-io/src/credit_ring_transport.rs)). DMA to the wire is zero-copy. |
| **write** (vectored, multi-slice) | **1** *(was 2)* | `send_gather` coalesces the slices **directly into the registered send MR** (`transport_common::gather_into`). The former `write_scratch` intermediate is removed. |
| **read** (hot path) | **1** | `poll_read_tokio`: `buf.put_slice(recv_buf(idx))` — recv MR → user `ReadBuf` ([async_stream.rs](../../rdma-io/src/async_stream.rs)). DMA into the MR is zero-copy. |
| **read** (write-blocked stash path) | **2** | `stash_recv` MR → owned `Vec`, later `put_slice` `Vec` → user buffer. |

### Data flow

```text
WRITE
  user &[u8] ──copy1──▶ registered send MR ──DMA(0-copy)──▶ wire
  (vectored) slices ──copy1──▶ write_scratch ──copy2──▶ send MR ──DMA──▶ wire

READ
  wire ──DMA(0-copy)──▶ recv MR ──copy1──▶ user ReadBuf
  (write-blocked) recv MR ──copy1──▶ recv_stash Vec ──copy2──▶ user ReadBuf
```

## Fundamental vs. reducible

### Fundamental (one copy each way)

RDMA can only DMA to/from a **registered** memory region. The
`AsyncRead`/`AsyncWrite` traits hand us unregistered user buffers
(`&[u8]` / `&mut [u8]`), so exactly one memcpy across that boundary is the price
of the byte-stream abstraction:

- **write:** `send_copy` must land the bytes in a registered send MR before
  posting the Send / RDMA-Write.
- **read:** the NIC DMA'd into the transport's recv MR; `poll_read` copies that
  into the caller's buffer.

### Reducible

1. **Vectored double-copy → single copy. — DONE.** `Transport::send_gather(&[IoSlice])`
   gathers the caller's slices (e.g. an HTTP/2 frame header + payload) *directly
   into the registered send buffer* via `transport_common::gather_into`; the
   stream's `write_scratch`/`MAX_GATHER` intermediate is gone. `send_copy`
   delegates to `send_gather` with a one-element list (byte-identical for the
   single-slice case, so the deadlock-critical ring path is unchanged). The
   default trait impl still coalesces-then-`send_copy` for mock transports.

2. **Small-message inline (send-recv only).** For `len <= max_inline_data`, post
   the Send WR **inline** directly from `data` (the provider copies into the WQE)
   and skip the MR memcpy. Does **not** apply to the ring transports: their
   RDMA-Write source must be a registered local buffer, so the copy is mandatory
   there.

3. **Write-blocked stash double-copy — intentional, leave it.** The eager
   copy-out into `recv_stash` is exactly what lets the write-blocked path call
   `repost_recv` immediately and keep freeing the peer's flow control. Removing
   it reintroduces the read-ring bidirectional deadlock (see
   [read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md)).
   It is also the rare backpressure path, not the hot path.

## Would `Buf` / `BufMut` / `Bytes` help?

Partially — the `bytes` crate is the right tool for the vectored case, not a
cure for the fundamental copies.

- **`Buf` on the write side (the real win).** A `poll_write_buf(&mut impl Buf)`
  that pulls the `Buf`'s chunks straight into the send MR collapses the vectored
  gather into a single copy, and matches how h2/tonic already hand data down
  (`Bytes` / chained buffers). It does **not** remove the MR copy — chunks still
  land in registered memory once. This is the clean way to realize reducible #1.

- **`Bytes` on the read side (bigger, architectural).** Wrap a recv MR slot as
  `Bytes` via `Bytes::from_owner`, with its `Drop` performing `repost_recv`. The
  consumer then gets a zero-copy, refcounted handle to the DMA'd bytes. Because
  prost decodes from `impl Buf` and h2 emits DATA frames as `Bytes`, this could
  thread **zero-copy reads** all the way to the application — **but only if the
  read path bypasses `tokio::io::AsyncRead`**, which forces `poll_read` into a
  user `&mut [u8]` (= a copy). hyper/h2 read through `AsyncRead`, so realizing
  this means replacing the read adapter with an `AsyncBufRead` / `Bytes`-returning
  path. Large change; gRPC-only payoff; easy to get repost/lifetime ordering
  wrong.

- **`BufMut` on the read side does not remove a copy** — it is still
  MR → `BufMut`.

## Recommendation (ranked)

1. **`send_gather` / `Buf`-based gather** — kill the vectored second copy. **Done**
   (see reducible #1): `Transport::send_gather` gathers straight into the send MR.
2. **Inline small sends (send-recv)** — skip the MR copy for tiny messages.
3. **`Bytes` zero-copy read path** — only if the gRPC read path is retargeted off
   `AsyncRead`; large and gRPC-specific.

All three are CPU-efficiency optimizations. Because the current ceiling is
completion/doorbell overhead and RTT (not memcpy), the higher-leverage transport
work remains unsignaled/batched sends and doorbell/completion batching.
