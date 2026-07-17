# RDMA mechanisms: Send/Recv, Write, Write+Imm, and Read

A conceptual primer on the RDMA data-path operations and how they work, so the
transport code in this repo (and the design docs) make sense. This is the
*background* companion to the design-level
[RdmaOperations.md](../design/RdmaOperations.md) (which analyzes the
performance trade-offs and why `AsyncRdmaStream` defaults to Send/Recv) and to
the three transport write-ups
([send-recv](../design/rdma-transport-layer.md),
[read-ring](../design/rdma-read-ring-transport.md),
[credit-ring](../design/rdma-credit-ring-transport.md)).

## Why RDMA is different

RDMA (Remote Direct Memory Access) lets one machine's NIC read/write another
machine's memory **without involving the remote CPU or kernel** on the data
path. Two properties drive everything below:

- **Kernel bypass.** The application talks to the NIC directly through mapped
  queues; there is no `read()`/`write()` syscall per message and no kernel TCP
  stack. (Setup — connection, memory registration — still uses the kernel.)
- **Zero-copy via registered memory.** The NIC DMAs straight into/out of
  application buffers, so there is no kernel socket-buffer copy. The buffers must
  be **registered** first (see below).

That is why RDMA wins CPU-efficiency and tail latency in the benchmarks
([grpc](../bench/grpc/README.md), [echo](../bench/echo/README.md)): the per-message cost
is a few doorbells and completions, not syscalls + a kernel stack traversal.

## The building blocks

```
                 application
                     │  posts Work Requests (WRs)          reaps completions
                     ▼                                          ▲
        ┌────────── Queue Pair (QP) ──────────┐         ┌──────────────┐
        │  Send Queue (SQ)   Recv Queue (RQ)  │ ──────► │ Completion Q │
        └─────────────────────────────────────┘         │    (CQ)      │
                     │  ring the doorbell                └──────────────┘
                     ▼
                    NIC ──── wire (InfiniBand / RoCE / iWARP) ────► peer NIC
```

- **Queue Pair (QP)** — the connection endpoint: a **Send Queue** (outbound
  operations) and a **Receive Queue** (buffers made available for incoming
  Sends). Roughly the RDMA analog of a socket. This repo uses **RC** (Reliable
  Connected) QPs — ordered, acked, retransmitted, like TCP.
- **Work Request (WR) / Work Queue Element (WQE)** — one posted operation
  ("write these bytes to that remote address", "here is a buffer to receive
  into"). Posting is a user-space memory write + a **doorbell** (an MMIO write
  that tells the NIC "new work"); no syscall.
- **Completion Queue (CQ)** — where the NIC posts a **Work Completion (WC)** when
  a WR finishes. You either **poll** the CQ or arm it for an **event** (wakeup).
  A completion carries status, byte count, and (for some ops) the *immediate*
  value.
- **Memory Region (MR)** — a range of application memory **registered** with the
  NIC so it can DMA to/from it. Registration pins the pages and yields two keys:
  - **lkey** (local key) — used by *your* WRs to name *your* buffers.
  - **rkey** (remote key) — handed to the peer so *its* one-sided Write/Read may
    target *your* memory. No rkey exchange, no one-sided access.
- **Protection Domain (PD)** — the trust boundary tying QPs and MRs together.

**Signaled vs unsignaled** — a Send-side WR can be *unsignaled* (no completion)
to save CQ traffic; you periodically post a signaled one to reclaim the queue.
Recv WRs always complete.

## The operations

There are two families: **two-sided** (both CPUs participate — the receiver must
have posted a buffer) and **one-sided** (the initiator's NIC does everything; the
target CPU is not involved).

### Send / Recv (two-sided)

Message passing. The receiver **must pre-post a Recv WR** (a buffer) *before* the
Send arrives; the NIC consumes one Recv WR per incoming Send and posts a recv
completion carrying the byte count.

```
sender                                   receiver
  post_recv(buf) ×N   (ahead of time) ──►  [RQ has buffers waiting]
  memcpy app→send_mr
  post_send(SEND) ─────── data ──────────► NIC DMAs into next posted buf
  send completion (SQ)                      recv completion (RQ): buf, len
                                            memcpy recv_mr→app
                                            post_recv(buf)   ← replenish
```

- **Notification:** yes — the recv completion *is* the signal. Natural for RPC.
- **Flow control is intrinsic:** the receiver paces the sender by how fast it
  re-posts buffers. If a Send arrives with **no** posted buffer → **RNR**
  (Receiver Not Ready): RoCE/IB retry with a backoff timer; **iWARP does not
  retry — it is a fatal disconnect** (hence this repo pre-posts a pool of recv
  buffers).
- **Cost:** two copies (app→send_mr, recv_mr→app) and both CPUs active.

### Write (one-sided)

The initiator writes bytes straight into a remote MR (named by address + rkey).
The **remote CPU is never involved** and gets **no notification** — the data just
appears in its memory.

```
initiator                                target
  post_send(WRITE, raddr, rkey) ── data ─► NIC DMAs into target memory
  send completion (local only)             (no completion, CPU unaware)
```

- **Use:** bulk push, log shipping, or writing into a ring the peer polls. Needs
  a side-channel to tell the peer *what* changed (that is exactly what
  Write+Imm, or a polled ring header, provides).

### Write with Immediate (one-sided data + a notification)

Like Write, but carries a small 32-bit **immediate** value and **consumes one
Recv WR** on the target, producing a **recv completion** there. So the data lands
zero-copy in the target's memory *and* the target's CPU is woken with the
immediate as an index/length/tag.

```
initiator                                target
  post_recv(doorbell) ×N (ahead)  ──────►  [RQ has "doorbell" WRs waiting]
  post_send(WRITE_WITH_IMM,                NIC DMAs data into target ring, and
            raddr, rkey, imm) ── data ───► consumes one doorbell WR →
                                           recv completion: imm (virtual idx/len)
```

- **Best of both:** zero-copy on the receive side (no `recv_mr→app` copy — the
  bytes are already in the ring) plus a wakeup. This is how **msquic** and
  **rsocket** move bytes (see [msquic-rdma.md](msquic-rdma.md),
  [Rsocket.md](Rsocket.md)).
- **Still needs pre-posted "doorbell" Recv WRs** to catch the immediates, so it
  is subject to the same RNR rule as Send/Recv if you run out of them.
- **Not on iWARP/siw** — Write+Imm is unsupported there.

### Read (one-sided pull)

The initiator pulls bytes *out of* a remote MR into its own buffer. Again the
**remote CPU is not involved** and only the **initiator** gets a (local)
completion.

```
initiator                                target
  post_send(READ, raddr, rkey) ◄─ data ── NIC DMAs from target memory
  send completion (local): data is here   (no completion, CPU unaware)
```

- **Use:** on-demand fetch, or reading a peer's counter — e.g. the read-ring
  transport pulls the peer's *consumed-head* pointer with a one-sided Read to
  learn how much ring space has freed up, without asking the peer to send
  anything.

### Atomics (one-sided)

`CompareAndSwap` (if `*addr == compare`, set `*addr = swap`) and `FetchAndAdd`
(`*addr += add`) on an **8-byte, 8-byte-aligned** location inside a remote MR
(named by address + rkey, like Write/Read). One-sided: the **target CPU is not
involved and gets no completion**; the *original* value is DMA'd back into the
initiator's local buffer and only the initiator gets a local completion.

Two caveats:

- **Atomicity scope is device-dependent.** RDMA atomics are atomic w.r.t. *other
  RDMA atomics on the same NIC* (`IBV_ATOMIC_HCA`) and only sometimes w.r.t. the
  target CPU's own atomics or a second NIC (`IBV_ATOMIC_GLOB`). Don't assume an
  RDMA `FetchAndAdd` and a local `fetch_add` on the same word are mutually
  atomic unless the platform guarantees it.
- **Not universally supported** — on the MANA RoCEv2 VMs here (and rxe/siw)
  atomics are effectively unavailable; the integration tests self-skip them. The
  byte-stream transports therefore use Write/Read/Send, not atomics, on the data
  path.

### At a glance

| Operation | Sides | Remote CPU | Notifies remote? | Zero-copy recv | iWARP |
|-----------|-------|-----------|------------------|----------------|-------|
| Send/Recv | two | yes (pre-post) | recv completion | no (1 copy) | ✅ |
| Write | one | no | no | n/a | ✅ |
| Write+Imm | one | yes (recv WR) | recv completion + imm | yes | ❌ |
| Read | one | no | local only | n/a | ✅ |
| Atomics | one | no | local only | n/a | partial |

(See [RdmaOperations.md](../design/RdmaOperations.md) for the performance
analysis and citations — FaSST/Demikernel — behind choosing two-sided vs
one-sided.)

## How the NIC writes the MR, and when the CPU may read it

When you register an MR the kernel **pins** the backing pages and hands the NIC
their physical addresses (+ keys). From then on the NIC writes your buffer by
**DMA over PCIe** — it never goes through the CPU or the kernel. On modern
platforms that DMA is **cache-coherent**: the PCIe root complex participates in
the coherence protocol (and with Intel **DDIO** the payload can land straight in
L3), so a core that later reads the buffer sees the new bytes with **no manual
cache flush or invalidate**. (A non-coherent platform would need explicit
barriers; not a concern on the x86/ARM servers here.)

The subtle part is **when** the bytes are safe to read, and it differs by op.

### Send/Recv and Write+Imm — the completion is the barrier

A message DMAs into the recv buffer as *several* PCIe transactions; while it is
in flight the buffer holds a **torn, partial** value. RDMA guarantees that by the
time you reap the **recv completion** from the CQ (`ibv_poll_cq`), the payload has
**fully landed and is visible to the CPU** — and that the *data* write is ordered
*before* the *completion* write. So the rule is simply:

> reap the completion first, **then** read the buffer.

You do **not** poll the buffer bytes, and you do **not** need an "atomic read":
once the completion is in, the NIC will not touch that buffer again until you
re-post it, so it is stable and a plain load is correct. (An RDMA *atomic* is a
different thing — a one-sided CAS/FAA op, see §Atomics — not how you read a
received message.)

The one ordering nuance: the load of the buffer must not be *speculated ahead of*
the load of the completion. `ibv_poll_cq` (and fast-path providers that read the
CQE straight from memory) insert the necessary **acquire barrier** between "saw
the CQE" and "read the data", so ordinary code gets this for free; only if you
hand-roll CQE polling do you owe the fence yourself. This is why the async layer
here reads received data only *after* `AsyncCq` returns the completion (see
[async_cq.rs](../../rdma-io/src/async_cq.rs)), never by peeking at the ring.

### Plain Write — no completion, so you poll the buffer

A one-sided Write produces **no remote completion**, so the CPU's only signal is
the memory itself — and the torn-during-DMA problem is now yours to solve: you
cannot trust "the last byte", you need a marker written *after* the data (a
separate flag Write on an RC QP, which RC orders after the payload) or an
intra-cacheline version/checksum. That is the memory-polling case in
[§Three ways to learn "the data arrived"](#three-ways-to-learn-the-data-arrived-memory-cq-kernel-event).

### Across cores

Once the reaping core has observed the completion (with the acquire above), cache
coherence makes the buffer globally visible. If you hand the buffer to another
thread, the completion read is the happens-before edge, so a normal handoff
(channel / lock / atomic flag) carries the visibility correctly — no special DMA
handling needed.

## Flow control and the receiver-not-ready trap

One-sided Write/Read cannot overrun anything by themselves, but the moment you
build a **byte stream** on top you need flow control so the sender does not
overwrite unread data or exhaust the receiver's doorbell WRs. The two ring
transports here solve it differently — and the difference is the crux of a
gotcha:

- **Read-ring (pull, silent):** the receiver advances a *consumed-head* offset in
  its own memory as it drains; the sender **pulls** that head with a one-sided
  **Read** before writing. The relief is silent — no message lands on the sender,
  so the sender must issue a Read to learn about it.
- **Credit-ring (push, self-notifying):** the receiver **sends credits back** as
  it drains, which land on the sender's CQ and wake it. The relief *is* a
  notification.

That "pull-based and silent vs push-based and self-notifying" distinction is why
read-ring had a subtle bidirectional stall under concurrent gRPC streams — see
[read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md).

## Three ways to learn "the data arrived": memory, CQ, kernel event

A one-sided **Write** (and an atomic's result on the *target*) produces **no
completion on the remote side** — the bytes just appear in registered memory. So
how does the receiver find out? There are three levels, and only the last one
touches the kernel:

1. **Poll the MR (the memory itself).** The receiver spins reading a flag /
   sequence number / tail pointer *in its own registered buffer*. Because the
   sender's Write DMA'd straight into that memory, the update is visible with
   **no verbs call, no CQ, no kernel** — the lowest-latency option and the only
   way to observe a *plain* Write. This is the standard polled-ring pattern
   (HERD, FaRM, rsocket, msquic-RDMA).
2. **Poll the CQ (`ibv_poll_cq`).** Also kernel-bypass and wakeup-free, but it
   requires the op to *generate a completion* — i.e. **Send/Recv** or
   **Write+Imm**, not a plain Write. Most designs spin here. `ibv_poll_cq` is a
   plain **userspace load of the CQ ring** (host memory the NIC DMAs CQEs into) —
   no syscall. A poll-only CQ needs **no completion channel and no fd** (create it
   with a null channel); the NIC raises **no interrupt** and touches no fd unless
   you *arm* it (below). So there is no wasted NIC/kernel work — the only cost is
   the spinning core.
3. **Kernel event (`ibv_get_cq_event`).** *Opt in* by arming the CQ
   (`ibv_req_notify_cq`): that asks the NIC to raise **one MSI-X interrupt** on
   the next completion, which the kernel turns into readability on the completion
   channel's **fd**; you block there (or epoll it) instead of spinning. This is
   the only path that needs the fd and the only one that enters the kernel; (1)
   and (2) avoid it, trading a **busy-spun core** for latency.

### Doing (1) correctly — the ordering trap

RDMA does **not** guarantee byte-order *within* a single Write, so you cannot
simply poll "the last byte" of the payload. Two robust patterns:

- **Separate flag Write after the data Write**, on the same **RC** QP: RC
  guarantees in-order message completion at the responder, so the flag becomes
  visible only after the payload has landed. (Write+Imm is the hardware-assisted
  version — the immediate *is* that ordered marker, but it consumes a recv WR and
  lands on the CQ, i.e. it converts case (1) into case (2).)
- **Intra-cacheline versioning** (FaRM-style): a version/checksum in the same
  64 B cacheline as the data, relying on PCIe writing a cacheline as a unit —
  poll the version and re-read on mismatch.

On coherent platforms (modern x86 + coherent PCIe DMA) the spinning CPU sees the
DMA'd update with no explicit flush; non-coherent setups need barriers.

### Why the transports here use the CQ, not memory polling

The ring transports could spin on their recv ring (case 1), but they use
**Write+Imm doorbell completions** (case 2) instead because they integrate with
**tokio/async**: async needs a *waker*, which a CQ event supplies cheaply, whereas
pure memory polling would need a dedicated spinning thread per connection — which
does not fit the many-connection async model. The read-ring transport does still
use case-(1)-style reads on the *sender* side: it issues a one-sided **Read** to
pull the peer's consumed-head value out of the peer's MR, learning how much space
freed up without asking the peer to send anything.

### Kernel bypass: which to poll, and busy-poll vs arm-and-park

For a kernel-bypass design the choice is **(1) vs (2)** — both stay in userspace;
only **(3)** enters the kernel. And it is not a free choice: it is dictated by
the data-path op.

- Data path is **Send/Recv or Write+Imm** (generates a completion) → poll the
  **CQ** with `ibv_poll_cq`. The completion gives you an ordered "data landed"
  signal *plus* the byte count, immediate, and error status for free, and **one
  CQ demultiplexes many QPs**, so a single poll loop serves many connections.
- Data path is a **plain Write** (no completion) → you have nothing to poll on
  the CQ; you **must** poll the **MR** with the ordering-marker discipline above.
  This wins the last ~100 ns of latency but you reimplement framing, length, and
  flow control yourself, and you need a spinner **per** buffer.

**Recommendation:** for a general transport, prefer **`ibv_poll_cq`** (case 2) —
simpler, correct-by-construction ordering, and it scales across connections.
Reach for MR polling (case 1) only for a bespoke single-stream, lowest-latency
path where you own the wire format.

**Busy-poll vs arm-and-park.** Pure kernel bypass means spinning `ibv_poll_cq`
with *no* arming and *no* `ibv_get_cq_event` — fully userspace, but it **burns
100% of a core**. That is fine for one or a few streams on a dedicated poller
thread; it does **not** scale to hundreds of connections in an async runtime (you
cannot spin a core per connection). So:

- **Dedicated low-latency service, few streams** → busy-poll `ibv_poll_cq`
  (case 2, no arming), or MR-poll (case 1) for the absolute floor.
- **Many connections / async** → `ibv_poll_cq` fast-path **plus an arm+event idle
  fallback** — kernel-bypass while completions flow, and 0 CPU (a kernel wakeup)
  only when a connection goes idle. That is exactly the drain-after-arm hybrid in
  [async_cq.rs](../../rdma-io/src/async_cq.rs) this library uses.

## How this library's transports map to the mechanisms

All three implement the same `Transport` trait and plug into `AsyncRdmaStream`
(the `AsyncRead`/`AsyncWrite` adapter), so they are drop-in swappable:

| Transport | Data path | Flow control | Notes |
|-----------|-----------|--------------|-------|
| **send-recv** (`SendRecvTransport`) | **Send/Recv** (two-sided) | receiver re-posts recv buffers | simplest, most portable (works on iWARP), 2 copies/msg — the default |
| **read-ring** (`ReadRingTransport`) | **Write+Imm** into a remote ring | one-sided **Read** of the peer's consumed-head | zero-copy recv; pull-based flow control |
| **credit-ring** (`CreditRingTransport`) | **Write+Imm** into a remote ring | **credit** returns from the receiver | zero-copy recv; push-based flow control |

Both rings pre-post small **doorbell** Recv WRs whose only job is to catch the
Write+Imm immediates (the immediate encodes a virtual index + length into the
ring); the payload bytes are already DMA'd into the ring, so the receive side
avoids the `recv_mr→app` copy that Send/Recv pays.

## Where to go next

- [RdmaOperations.md](../design/RdmaOperations.md) — perf analysis, why Send/Recv
  is the default data path.
- [rdma-transport-comparison.md](../design/rdma-transport-comparison.md) —
  three-way comparison of the transports.
- [rdma-read-ring-transport.md](../design/rdma-read-ring-transport.md) /
  [rdma-credit-ring-transport.md](../design/rdma-credit-ring-transport.md) — the
  ring designs in depth.
- [Rsocket.md](Rsocket.md), [msquic-rdma.md](msquic-rdma.md) — prior art for
  Write+Ring byte streams.
- [read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md)
  — how the silent pull-based flow control bit us and how it was fixed.
