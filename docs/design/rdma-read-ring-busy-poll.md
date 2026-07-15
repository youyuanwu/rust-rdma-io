# RDMA Read Ring — Arm-Park and Busy-Poll Completion Modes

**Status:** Implemented (experimental) — Phase 0 seam + Phase 1 busy-poll (read-ring) landed and MANA-validated; current behavior lives in code, remaining hardening is in §13 (review must-fix items 6/8/13 addressed 2026-07-15).  
**Date:** 2026-07-10 (design)  
**Builds on:** [rdma-read-ring-transport.md](rdma-read-ring-transport.md) — `ReadRingTransport` (wire/data path reused wholesale)  
**Background:** [../background/RdmaMechanisms.md](../background/RdmaMechanisms.md) — *§Kernel bypass: which to poll, and busy-poll vs arm-and-park*  
**Contrast:** [rdma-transport-layer.md](rdma-transport-layer.md), [`async_cq.rs`](../../rdma-io/src/async_cq.rs) — current arm-and-park model

## 1. Motivation and the two modes

Today every transport reaches the CQ through [`AsyncCq`](../../rdma-io/src/async_cq.rs), which
implements an **arm-then-drain** hybrid: it *arms* the CQ (`ibv_req_notify_cq`), then drains with
`ibv_poll_cq`, and if the CQ was empty it parks on the completion-channel **fd** via tokio's
reactor, woken by an **MSI-X interrupt → epoll** round trip. That is the right default — it drops
to **0 % CPU when idle** and scales to hundreds of connections on a multi-threaded runtime — but
the arm/interrupt/epoll path adds **microseconds** of wakeup latency on every transition through
idle, and the reactor hop costs a syscall.

This document adds a **second, co-equal completion mode** for the **read-ring** data path and,
crucially, keeps the first. **Both modes are first-class and supported:**

- **Arm-park (existing default).** Interrupt-driven, 0 % idle CPU, one CQ pair per connection on a
  shared multi-threaded runtime. Unchanged.
- **Busy-poll (new).** Kernel-bypass on the completion path: a per-core driver **spins
  `ibv_poll_cq`** (a userspace load of the CQ ring — see
  [RdmaMechanisms §Three ways to learn "the data arrived"](../background/RdmaMechanisms.md#three-ways-to-learn-the-data-arrived-memory-cq-kernel-event)),
  never arming the CQ on the data path. Thread-per-core, connections sharded to cores, for the
  **lowest wakeup latency and per-op CPU** when you can dedicate cores.

The two modes are unified by a single seam — a **completion source** (§3) that both implement —
so the read-ring wire protocol, ring layout, immediate encoding, flow control, `CompletionTracker`,
and the `AsyncRdmaStream` adapter are **identical** in both, and the choice is a construction-time
config, not a fork of the transport.

| | **Arm-park** (existing) | **Busy-poll** (new) |
|---|---|---|
| Completion discovery | arm CQ → interrupt → epoll → `ibv_poll_cq` | per-core driver spins `ibv_poll_cq` |
| CQ ownership | one send + one recv CQ **per connection**, owned by the transport | one send + one recv CQ **per core**, owned by a `CoreDriver` |
| Idle CPU | 0 % | 100 % of the polling core (until the deferred hybrid, §5.4) |
| Wakeup latency under load | interrupt + epoll (µs) | userspace poll (ns) |
| Runtime | multi-thread, `Send` tasks | N pinned `current_thread` runtimes, connections sharded |
| Sole CQ reaper | the transport (its own CQ) | the `CoreDriver` (the shared CQ) |
| Best for | general use, many connections, spiky load | dedicated latency-critical service, cores to spare |

Read-ring is the natural first target: its receive path already reaps **Write+Imm doorbell
completions** off a CQ (case 2 in the background doc), and its send-side flow control is a silent
one-sided **Read** whose completion also lands on the send CQ. There is no per-message interrupt
to begin with — only the *idle wakeup* uses one — so busy-poll changes only *who polls and how a
parked future is woken*, never the wire.

**Non-goals.** No new wire format, no new flow-control scheme, and no removal of arm-park.
Busy-poll is an alternate *completion driver* selected by configuration.

## 2. What stays the same — read-ring reuse

The read-ring **data path and flow control** are reused unchanged in **both** modes. The mode
difference is confined to the completion source (§3) and, for busy-poll, the runtime topology and
connection lifecycle (§4–§7).

| Read-ring component | Location | Reuse |
|---|---|---|
| Ring buffers (send/recv), wrap/padding | `read_ring_transport.rs`, `transport_common.rs` | **unchanged** |
| Token exchange (v2: offset-buf VA + rkey) | `transport_common.rs` | **unchanged** |
| MW binding (recv `REMOTE_WRITE`, offset `REMOTE_READ`) | `read_ring_transport.rs` | **unchanged** |
| Immediate encode/decode (offset+len), virtual index map | `transport_common.rs` | **unchanged** |
| `CompletionTracker` (out-of-order slot release, chase-forward) | `transport_common.rs` | **unchanged** |
| `send_gather` → `WRITE_WITH_IMM`, padding, `memcpy` | `read_ring_transport.rs` | **unchanged** |
| Doorbell / RNR backpressure cap (`SEND_COPY_MAX_WRS`) | `read_ring_transport.rs` | **unchanged** |
| One-sided **Read** head refresh (`post_offset_read`, liveness) | `read_ring_transport.rs` | **unchanged** |
| Completion **interpretation** (Read-sentinel, imm decode, tracker advance) | `read_ring_transport.rs` | **unchanged — stays in the transport in both modes** |
| `AsyncRdmaStream<T>` byte-stream adapter, write-blocked drain/stash | `async_stream.rs` | **unchanged** |
| `Transport` trait surface (`send_gather`, `poll_recv`, …) | `transport.rs` | **unchanged** |
| **Where completions come from** (own `AsyncCq` vs driver inbox) | new `CompletionSource` seam (§3) | **new** |
| **Runtime / lifecycle** (per-core driver, shared CQ, setup/teardown) | new `CoreDriver` (§4–§7) | **new, busy-poll only** |

One correction to an earlier framing: this is **not** "just add a mode flag to `AsyncCq`."
`AsyncCq` **arms first, then drains** — a busy-poll path that skips arming must branch *before*
`ibv_req_notify_cq`, and under a shared per-core CQ the transport must **not** call `ibv_poll_cq`
at all (that would steal other connections' completions — the multi-reaper bug read-ring already
fought). The real seam is a small **completion-source abstraction** the transport reads from;
`AsyncCq` becomes one implementation of it.

## 3. The completion-source seam

Both modes present the transport with the same thing: *"give me the next batch of
`WorkCompletion`s for this direction, or take my waker and return `Pending`."* The transport then
applies its **unchanged** interpretation (decode immediate, advance `CompletionTracker`, handle
the Read sentinel, release ring space). Interpretation never moves; only *acquisition* differs.

```rust
/// How a transport acquires raw completions for one direction (send or recv).
/// The transport still interprets them; this only delivers them.
trait CompletionSource {
    fn poll_completions(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut [WorkCompletion],
    ) -> Poll<Result<usize>>;
}
```

Two implementations, chosen at construction (an enum keeps dispatch static — no vtable — since
there are exactly two):

- **`ArmParkSource`** — wraps today's `AsyncCq` (owns a CQ + completion channel +
  `TokioCqNotifier`). `poll_completions` is exactly the current arm-then-drain path. It is the
  **sole reaper of its own per-connection CQ**. Existing behavior, verbatim.
- **`DriverSource`** — holds an `Arc<ConnSlot>` registered with the core's `CoreDriver`. It never
  touches a CQ; `poll_completions` drains the slot's per-direction **inbox** (a bounded queue the
  driver fills) and, when empty, stores `cx.waker()` in an `AtomicWaker` and returns `Pending`.
  The **`CoreDriver` is the sole reaper** of the shared CQ (§5.1).

The transport holds a `send_src` and a `recv_src`, each one of the two variants.
`poll_send_completion` and `poll_recv` call the source, then run the same read-ring logic on the
returned completions. This is why both modes share one transport type and one interpretation path.

**The seam must reach into `AsyncQp` and the setup helpers, not just the transport.** Today
`AsyncQp` owns concrete `AsyncCq`s and exposes `send_cq()`/`recv_cq()`
([async_qp.rs](../../rdma-io/src/async_qp.rs)), and connection *setup* drains completions through
the QP before the transport exists. For busy-poll that raw access must go away: a busy-mode QP is
created against the driver's shared CQs and **must not expose any `send_cq()`/`recv_cq()` or other
raw CQ-polling path** to transport or setup code. Every wait — setup token/MW-bind completions and
the data path alike — goes through a `CompletionSource`. Concretely, either split posting (the QP)
from completion acquisition, or make `AsyncQp` completion-source-aware; the arm-park QP keeps its
own `AsyncCq`-backed source, so this is additive.

## 4. Thread-per-core architecture (busy-poll mode)

### 4.1 Layout

```
   accept / connect ───────────►  shard by connection → owning core
                                     (a connection lives on ONE core for life)

  ┌── core 0 (pinned) ──────────┐   ┌── core 1 (pinned) ──────────┐   ...
  │ tokio current_thread rt      │   │ tokio current_thread rt      │
  │  (IO + time drivers ON)      │   │                              │
  │                              │   │                              │
  │  CoreDriver task             │   │  CoreDriver task             │
  │   sole reaper: ibv_poll_cq   │   │   ...                        │
  │   demux wc.qp_num → ConnSlot │   │                              │
  │   push inbox, wake AtomicWkr │   │                              │
  │   bounded work, yield_now    │   │                              │
  │                              │   │                              │
  │  conn A  conn B  conn C ...  │   │  conn D  conn E ...          │
  │  (Arc<ConnSlot> + Async-     │   │                              │
  │   RdmaStream + app)          │   │                              │
  │     send/recv CQ (per core)  │   │                              │
  └──────────────────────────────┘   └──────────────────────────────┘
       shared ibv Context (Arc) ─────────────► NIC
```

- **One RDMA device `Context`** (`Arc<Context>`) is shared read-only across cores, and the **two
  shared CQs are per core**, but **PD, QP, MRs, and rings stay per connection** (unchanged from
  today). A per-core PD is *rejected*: on MANA, where MWs are unavailable and raw MR rkeys are
  exposed, sharing one PD across unrelated connections weakens the isolation boundary (§7.4). CQ
  sharing does **not** require PD sharing — QPs in different PDs can share a CQ. A connection's QP,
  ring, PD, and `Arc<ConnSlot>` live on its owning core for life.
- **Same-core, not lock-free-by-magic.** The driver and app tasks run on one thread, so there is
  no cross-core contention, but the `CoreDriver` and the transport are **distinct tasks** sharing
  a `ConnSlot`. That sharing is expressed with `Arc<ConnSlot>` + bounded queues + `AtomicWaker`
  (§7.1), not `Rc<RefCell<…>>` — so the transport type stays `Send + Sync` and works in *both*
  modes. Same-core uncontended atomics are cheap; this is a correctness/type choice, not a perf
  regression.
- **Core pinning is Phase 1, not later.** Each worker is pinned (`core_affinity` /
  `sched_setaffinity`) **before** it allocates its CQs and MRs, so the poll loop, CQ ring,
  doorbell MMIO, and DMA buffers are node-local and latency numbers are valid. SMT-sibling
  avoidance and least-loaded sharding are the only affinity items deferred.

### 4.2 One shared CQ pair per core, demultiplexed by QP

In busy-poll a **single send CQ + single recv CQ per core** are shared by every QP on that core,
so the driver polls **exactly two CQs regardless of connection count**. Each connection registers
`qp_num → Arc<ConnSlot>` with the driver; the driver routes each `WorkCompletion` by `wc.qp_num`
into the slot's send or recv inbox. Read-ring already carries everything the recv side needs in
the **immediate** + `CompletionTracker`, so the driver only *routes* raw completions — it never
interprets them.

The shared CQs are created **poll-only**: `ibv_create_cq` with a **null completion channel**, so
there is no fd and the NIC raises no interrupt on the data path (see
[RdmaMechanisms §2](../background/RdmaMechanisms.md#three-ways-to-learn-the-data-arrived-memory-cq-kernel-event)).
QPs are created against these shared CQs instead of minting per-connection CQs.

`qp_num` can be **reused** after a QP is destroyed; a stale in-flight CQE for a retired QP must
not be mis-routed to a new connection. A `WorkCompletion` exposes only `qp_num` — **no software
generation** — so a CQE cannot be validated against a generation at poll time; a map lookup by
`qp_num` would just return the *current* occupant. Therefore the **verified retirement barrier
(§6.2) is the sole protection**: a `qp_num` is not removed from the map (and not reusable) until
that connection's counters are zero and its inboxes empty, which guarantees no CQE for the old QP
can still be in the CQ. Generations tag only *software* handles (slot handles, reclaim commands,
already-routed inbox entries), never raw CQEs. Consequently a CQE for an **unknown `qp_num` is a
fatal invariant violation** (the barrier was breached), not something to "drop safely."

## 5. The driver: sole CQ reaper + cooperative loop

### 5.1 The sole-reaper rule

**In busy-poll, the `CoreDriver` is the only code that calls `ibv_poll_cq` on the shared CQs, and
the transport never touches them.** This resolves the central design finding: the transport's
`poll_send_completion`/`poll_recv` go through `DriverSource`, which reads the slot inbox and never
polls a CQ. Interpretation (Read-sentinel, imm decode, tracker) still runs in the transport,
exactly once, on the completions the driver delivered.

This preserves the read-ring **single-owner send-CQ invariant**
([read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md)): the
driver is that single owner. It must route the one-sided **Read** head-refresh completion to the
owning slot so the transport applies `update_cached_remote_head` / clears `read_in_flight` exactly
once, and it must wake the write-blocked future afterward. Two reapers (driver *and* transport) is
the same bug in a new costume — hence the transport is forbidden from polling the shared CQ.

### 5.2 The cooperative loop (bounded work)

The `CoreDriver` runs as **one task per core** on that core's `current_thread` runtime — never one
per connection. Because the transport and `ConnSlot` are `Send + Sync` (§7.1) it is an ordinary
`tokio::spawn` task; **no `LocalSet`/`spawn_local` is required** (that would only be needed under
the rejected `!Send` fork). The per-core `current_thread` runtime — not the multi-thread
work-stealing runtime — is what keeps the driver, its CQ, and its connections pinned to one core.

```text
loop {
    let mut drained = 0;
    for cq in [recv_cq, send_cq] {
        // Bounded drain — never let one CQ monopolize the sweep.
        let n = cq.poll(&mut wc_batch[..MAX_CQE_PER_CQ]);   // ibv_poll_cq, userspace load
        for wc in &wc_batch[..n] {
            match slots.get(wc.qp_num) {
                Some(slot) => {
                    slot.inbox(wc.dir()).push(*wc);          // bounded queue
                    slot.waker(wc.dir()).wake();             // AtomicWaker
                }
                None => fatal_unknown_qp(wc),                // post-barrier this is impossible (§4.2)
            }
            drained += 1;
        }
    }

    // Cooperative yield after each sweep (and always within a bounded budget):
    // hand the core to app tasks, then come back. Never parks in pure busy-poll.
    tokio::task::yield_now().await;
    if drained == 0 { /* optional: deferred idle fallback, §5.4 */ }
}
```

- **Bounded work, not just `yield_now`.** Each sweep drains at most `MAX_CQE_PER_CQ` per CQ (and a
  `MAX_CQE_PER_TURN` overall), then yields. This bounds the gap between a task's turns even when
  one connection floods completions, which `yield_now` alone does not guarantee (tokio's
  current_thread scheduler is not strict round-robin). See §9.
- **The app never polls a CQ.** `poll_recv`/`poll_send_completion` drain their inbox; if empty they
  register in the slot's `AtomicWaker` and return `Pending`. Producer = driver, consumer =
  transport task, both same-core.
- **Local wakes.** `waker.wake()` is a same-thread run-queue push — no `mio` unpark, no cross-core
  atomic signal.

### 5.3 Tokio integration: integrated busy-poll (not syscall-free)

An earlier draft claimed a permanently-ready driver makes the runtime **never park**, freezing
timers and the CM fd. That is **not** how current_thread tokio behaves: the scheduler performs a
**periodic non-blocking driver turn** (its `event_interval`, default 61 task polls) even while
tasks stay ready, so `tokio::time` timers and `AsyncFd` readiness (the CM disconnect channel
behind `poll_disconnect`) **do** make progress under a busy driver.

The adopted model is therefore **tokio-integrated busy-poll**: build each worker as a
`current_thread` runtime **with IO and time drivers enabled**. The driver spins the CQ and yields;
tokio's periodic reactor turn services CM and timers with no special handling. Consequences:

- **No per-sweep CM draining.** The earlier `drain_cm_events_nonblocking()` step is **removed** —
  it would be O(N) `try_get_event` syscalls per sweep across N per-connection CM channels, exactly
  the scaling the shared CQ avoids. CM stays on the existing `AsyncFd` path (`poll_disconnect`
  unchanged), serviced by the periodic reactor turn.
- **Not strictly syscall-free.** The core makes occasional reactor syscalls on the
  `event_interval` cadence. That is fine and is the price of keeping timers/CM correct. Because the
  behavior relies on tokio's `event_interval`, the **tokio version and scheduler config are part
  of the contract** (§7.2).
- **Strict syscall-free is a deferred alternative.** A worker built with **no** IO/time driver
  would be truly syscall-free but must move CM and timers to a separate control plane. Deferred;
  only for a dedicated latency benchmark.

### 5.4 Bounded-spin idle fallback (DEFERRED)

> **Deferred.** The initial busy-poll scope is **pure spin** — the driver always `yield_now`s and
> never arms. The hybrid below is captured as follow-up, not built now.

Pure spin pins the polling core at 100 % even when idle. The deferred **bounded-spin hybrid**
would busy-poll while completions arrive (or within a `SPIN_BUDGET` of empty sweeps), then arm the
shared CQ and park on its completion channel so an idle core returns to 0 % CPU — the same
philosophy as today's `AsyncCq` arm-then-drain, reusing that code for the fallback. (This is
orthogonal to §5.3: timers/CM already work via the periodic reactor turn; the hybrid is purely
about idle **CPU**, not correctness.) Deferring it keeps the first cut free of spin↔park hysteresis
tuning.

| Mode | Idle CPU | Wakeup latency under load | Timers/CM | Status |
|---|---|---|---|---|
| Arm-park (today) | 0 % | interrupt + epoll (µs) | native | shipped default |
| Busy-poll, pure spin | 100 % / core | userspace poll (ns) | native via periodic reactor turn | **initial busy-poll scope** |
| Busy-poll + bounded-spin hybrid | 0 % when idle | userspace poll (ns) while hot | native | **deferred follow-up** |

## 6. Connection lifecycle on a shared CQ

A shared, poll-only CQ changes both connection **setup** and **teardown**: neither can privately
drain a CQ any more (that would consume another connection's completions), and `Drop` cannot
synchronously flush. Both become **driver-mediated**.

### 6.1 Setup handoff (Connecting slot)

Today `ReadRingTransport::connect/accept` posts setup WRs (token exchange, MW binds) and drains
their completions with `drain_send_cq` before the transport exists. On a shared CQ those
completions land in the core's CQ and must be routed. So:

1. Create the QP **against the core's shared CQs** and register a **provisional `ConnSlot` in
   `Connecting` state** with the driver **before** posting any setup WR.
2. Post token send/recv and MW-bind WRs; the driver routes their completions to the slot inbox
   like any other. The setup future drains the inbox (not `drain_send_cq`).
3. On success, transition the slot `Connecting → Established` atomically and hand the built
   transport to the app task. On failure (timeout, token/MW error, rejection) run the teardown
   barrier (§6.2) from the `Connecting` state — it must clean up half-open QPs too.

**Server side** needs a precise, **CM-ID-keyed** routing state machine (matching on the *event's
CM ID*, not merely the event type — otherwise concurrent accepts associate an `Established` with
the wrong setup future):

1. A single **control task** is the sole consumer of the **listener's** CM event channel; it keeps
   a `pending: HashMap<raw CmId, PendingConn>` for in-flight accepts.
2. On `ConnectRequest`, the control task picks the owning core and **dispatches resource creation
   to that worker**: the worker creates the per-connection PD/QP/MRs on its core against its shared
   CQs and registers the `Connecting` slot, then **acknowledges** back to the control task.
3. Only after that ack does the control task call `accept` for that CM ID.
4. An incoming `Established` / `Disconnected` is matched by its **CM ID** against `pending`, then
   that exact `CmId` is **migrated to a per-connection event channel** and handed to the owning
   worker for `poll_disconnect`.

PD/QP/MR creation thus happens **after** core selection (node-local) and **before** the `accept`
reply that lets the peer start sending.

### 6.2 Teardown barrier: ownership transfer + forced flush + accounting

`Drop` can no longer poll the CQ (it is shared), and it must **not** RAII-destroy the QP/MRs while
completions for them may still be in that CQ. So the resources are **handed to the driver** and
close becomes an async protocol, not a synchronous drain.

**Ownership transfer.** Busy-mode RDMA resources live in a transferable **`ResourceBundle`** — an
`Option`-held `ReadRingInner` (one plain struct owning the QP, MWs, MRs, PD, CM ID + event channel,
and the completion sources; see §10) beside the `Arc<ConnSlot>` (routing key + outstanding-WR
accounting) that outlives it. `Drop` (and cancel / setup-failure / panic) `take()`s the bundle and
enqueues it on the driver's **reclaim queue** — RAII destroys *nothing* itself. (Spawning an async
cleanup task from `Drop` is insufficient: the runtime may already be shutting down; it must be a
plain queue the driver owns.) At **runtime shutdown** the driver reclaims *every* bundle before the
shared CQs and per-core context are torn down.

**Forced flush — the barrier must terminate.** `disconnect()` today is *only* an RDMA-CM
disconnect ([read_ring_transport.rs](../../rdma-io/src/read_ring_transport.rs)); it does **not**
move the local QP to error, so outstanding WRs are not guaranteed to flush and the barrier could
hang. The protocol per bundle is therefore:

1. Mark the slot `Closing`; stop posting new WRs.
2. Optionally initiate graceful CM disconnect, then **explicitly transition the QP to
   `IBV_QPS_ERR`** via a safe `to_error()` API on the CM'd QP (not a raw-pointer poke). This forces
   every outstanding send/recv WR to complete as a success or `WR_FLUSH_ERR` CQE.
3. The driver keeps routing this QP's CQEs to the bundle and **decrements the accounting** on each,
   under a **cleanup timeout / forced-failure** policy if the transition or drain stalls.
4. When accounting reaches the zero barrier (below), the slot goes `Closing → Drained`.
5. Only now destroy **MWs → QP → MRs** in that order (invalidate/dealloc the bound MWs *before* the
   QP, then the QP releases its MR refs, then deregister the MRs — encoded in the bundle's drop, not
   left to prose), then **retire** the slot (remove `qp_num`, bump generation). `qp_num` reuse is
   legal only after retirement.

**Outstanding-WR accounting.** `send_in_flight` alone is insufficient — the barrier must count
*everything* the NIC can still complete: setup WRs (token send/recv, MW binds), data-path
Write+Imm, the one-sided **Read**, doorbell recvs (posted until teardown), and reposts. So:

- **Separate send and recv counters**, started **before the first setup WR**.
- Increment **only after** a successful post; decrement **once per** CQE, success *or* flush.
- **No `saturating_sub`** — underflow is an invariant violation (fatal), not something to clamp.
- `Drained` requires **both counters zero and both inboxes empty**.
- The driver must deliver flush CQEs to this state machine **even if the app task is already gone**
  — which is exactly why the driver, not the app, owns the bundle during reclaim.

## 7. Type, sizing, and waker contracts

### 7.1 `Transport: Send + Sync` is preserved (a deliberate tradeoff, not a hard limit)

Only one thing is shared between the **driver task** and the **transport/app task**: the
`ConnSlot` (per-direction inboxes, wakers, and outstanding-WR/lifecycle state). The QP, MRs, MWs,
and rings are single-task; the shared CQ and `qp_num → ConnSlot` map are the driver's; only
`Arc<Context>` is shared across cores.

Because the driver and transport run on the **same thread**, `Rc<RefCell<ConnSlot>>` +
`Cell<Option<Waker>>` would be perfectly memory-safe (borrows never overlap, and `LocalSet` /
`spawn_local` accept `!Send` tasks) and would need **no atomics at all**. It is not impossible —
it is *rejected on purpose*, because `Rc`/`RefCell` are `!Send`, which makes the transport type
`!Send` and **violates the `Transport: Send + Sync` bound** ([transport.rs](../../rdma-io/src/transport.rs)).
That bound is load-bearing: the arm-park path runs on a **multi-thread runtime**, and
**tonic/hyper require `Send` streams** (`AsyncRdmaStream<T>` is `Send` only if `T: Send`). Using
`Rc<RefCell>` would force either dropping `Send + Sync` from the trait (breaking every existing
consumer) or **forking a separate `!Send` `LocalTransport` trait + adapter**.

So the tradeoff is:

- **Chosen — one `Send + Sync` type for both modes.** Share the `ConnSlot` via `Arc`, use a
  **bounded** SPSC-style queue per direction and an `AtomicWaker` per direction. Here `AtomicWaker`
  (over a bare `Cell<Waker>`) is required *because the slot is `Sync`* — not because of a
  same-thread data race — and it keeps register/wake lost-wakeup-safe. Cost: cheap same-core
  **uncontended** atomics.
- **Rejected — a `!Send` `LocalTransport` fork.** `Rc<RefCell>` + `Cell<Waker>`, no atomics,
  slightly simpler locally, but it duplicates the trait, the `AsyncRdmaStream` adapter, and the
  read-ring interpretation path, and it cannot be reused by the arm-park / tonic consumers.

The single-type choice keeps the seam (§3) honest: the *same* `ReadRingTransport` runs under both
modes, differing only in which `CompletionSource` variant it holds.

### 7.2 Shared-CQ sizing and admission control

Per-connection CQ depths do not translate to a shared CQ. Busy-poll needs:

- **Aggregate CQE sizing.** `recv_cq` depth ≥ Σ per-connection doorbell WRs on the core; `send_cq`
  depth ≥ Σ (outstanding Write+Imm + one Read) on the core — both validated against the device's
  `max_cqe`.
- **Admission control.** A per-core **connection cap** derived from those sums; refuse or redirect
  a new connection that would overrun the CQ *even though each QP is individually within its WR
  limits*.
- **Reserved flush headroom** so a disconnect burst (all of a QP's WRs flushing at once) cannot
  overrun the shared CQ.
- **Inbox sizing is a correctness requirement, not backpressure.** Once the driver has reaped a
  CQE it *cannot* return it to the hardware CQ, and a recv CQE's **immediate is the ring framing**
  (offset/len) — dropping it corrupts the stream. So an inbox may **never** overflow: size each
  per-direction inbox to that QP's **maximum simultaneously-completable WRs** (data + Read + setup
  + doorbell + flush), which is inherently bounded by the QP's WR budget, so it is provably
  overflow-free (equivalently, a driver-owned non-dropping overflow queue). If overflow is ever
  observed it is a **fatal, per-connection** fault: retain the triggering CQE, mark only that
  connection fatal, transition its QP to `ERR`, wake both directions, and keep reaping/accounting
  its flush CQEs. Unknown/stale-QP completions and CQ overrun are counted and fatal (§4.2).

### 7.3 Waker semantics

`ConnSlot` carries **separate read and write** waiters, each an `AtomicWaker`, with:

- **Register–check–recheck** ordering in `poll_*` (store waker, then re-drain the inbox once) to
  close the completion-arrived-just-before-registration race.
- **Wake both directions** on disconnect / fatal WC, so a task blocked on the other half unblocks.
- **Coalesced wakes** — multiple CQEs in one sweep wake a direction at most once.
- **Single waiter per direction** (the `AsyncRdmaStream` read half and write half); documented as
  an invariant.
- Close/cancel wakes both directions and thereafter returns EOF/`BrokenPipe` per the existing
  `Transport` contract.

### 7.4 Per-connection PD (isolation)

Busy-poll keeps the **per-connection PD** the transports use today
([read_ring_transport.rs](../../rdma-io/src/read_ring_transport.rs) allocates a PD per `cm_id`) and
explicitly does **not** move to a per-core PD. The reason is MANA: MWs are unavailable there, so
the data path falls back to exposing **raw MR rkeys** to the peer. A PD is the trust boundary that
scopes which rkeys a QP may use; sharing one PD across unrelated connections on a core means a
leaked or guessed rkey is valid against *other* tenants' MRs. CQ sharing delivers the busy-poll
scaling win and does **not** require PD sharing — QPs in different PDs can post to the same shared
CQ. A per-core PD may be offered only as an opt-in optimization among *mutually trusted*
connections, documented as a security tradeoff.

### 7.5 Worker-affinity guard

Keeping the transport `Send + Sync` (§7.1) is convenient but it also *permits* a caller to move a
busy-mode transport to another thread/runtime, where touching its QP/CQ (ibverbs objects are not
thread-safe without external sync) would race. `current_thread` only guarantees affinity while the
object stays inside its runtime. So busy-mode state carries an **owner-worker token** checked
(a cheap `debug_assert` / fail-fast) on every post, completion poll, disconnect, close, and the
`Drop` resource transfer. Documentation alone cannot enforce QP/MR thread-affinity; the guard
catches misuse immediately instead of as a heisenbug.

## 8. Correctness invariants carried over from read-ring

Busy-poll must actively preserve the read-ring invariants — a hotter loop turns "unlikely" races
into "every iteration":

- **Single owner of the send CQ** — now the `CoreDriver` (§5.1). The transport must never poll the
  shared CQ; the driver routes the Read-sentinel and Write completions so the transport updates
  flow control exactly once and wakes the write-blocked writer.
- **Doorbell / RNR cap unchanged.** `SEND_COPY_MAX_WRS`-vs-`max_outstanding` and the proactive
  `post_offset_read` liveness heartbeat are unchanged; they still guarantee a send-CQ completion
  when doorbell-blocked, which the driver relies on to re-poll a parked writer.
- **Write-blocked recv drain still runs.** `AsyncRdmaStream`'s stash/drain (releasing the peer's
  flow control while the local writer is blocked) is unchanged and keeps the bidirectional stream
  deadlock-free.
- **Completion→data ordering.** The acquire barrier in `ibv_poll_cq` still orders "saw the CQE"
  before "read the ring bytes" (background doc §*How the NIC writes the MR*); the transport reads
  ring bytes only from completions the driver delivered, so ordering holds. Busy-poll changes
  polling *frequency*, not ordering.
- **Zero-outstanding teardown barrier** (§6.2) is itself an invariant: no QP/MR is destroyed while
  a CQE for it may still be in the shared CQ.

## 9. Fairness and backpressure

`yield_now` alone is not a fairness contract — tokio's current_thread scheduler does not guarantee
strict driver→app round-robin, and app futures are cooperative. So:

- The driver drains **bounded** work per turn (`MAX_CQE_PER_CQ`, `MAX_CQE_PER_TURN`) then yields
  (§5.2), bounding the poll-gap even when one connection floods.
- A CPU-heavy app future (large TLS record, big `memcpy`) can still delay the next sweep; this is
  an **application contract** (keep per-poll work bounded) and is measured by the max poll-gap
  metric (§12).
- Inboxes are **sized so they cannot overflow** (§7.2), so they are not a backpressure mechanism;
  per-connection backpressure remains the read-ring flow control (doorbell/credit + the
  remote-ring space check), and admission control (§7.2) bounds aggregate load per core.

## 10. Phasing

### What is implemented (and where the code lives)

Phase 0 (the completion-source seam) and Phase 1 (busy-poll end-to-end for read-ring,
correctness-complete) are **landed and MANA-validated**. The phasing/slice narrative that guided the
build has been removed in favor of the code, which is now the source of truth:

| Piece | Code |
|---|---|
| Completion-source seam (`ArmPark` + `Driver`) | [`completion_source.rs`](../../rdma-io/src/completion_source.rs) |
| Poster QP (`new_poster`, `to_error()`, no raw CQ accessors) | [`async_qp.rs`](../../rdma-io/src/async_qp.rs) |
| Per-core shared-CQ reaper, `qp_num` demux, reclaim barrier | [`core_driver.rs`](../../rdma-io/src/core_driver.rs) |
| `ConnSlot` (inboxes, wakers, WR counters, lifecycle state) | [`conn_slot.rs`](../../rdma-io/src/conn_slot.rs) |
| Busy connect/accept, `ReadRingInner` split, reclaim handoff | [`read_ring_transport.rs`](../../rdma-io/src/read_ring_transport.rs) |
| `BusyPool` / `ArmParkPool` (pinned pool, sharding, admission, CQ sizing) | `rdma-io-busy/src/lib.rs` |
| Bench modes (`echo-busy`, `rh1-busy`, `rh1-park`) | `tests/rdma-io-bench/src/` |

The **arm-park default is unchanged and remains the regression baseline.** The three prerequisites
also shipped: **P1** the idempotent `to_error()` QP-to-`ERR` primitive; **P2** the deterministic,
accounted teardown drain (a latent-bug fix that replaced the best-effort single drain); **P3** the
CQ-ownership decoupling that slimmed `AsyncQp` to a poster and moved completion acquisition entirely
into `CompletionSource`. Read the code and its tests for exact behavior.

### Remaining phases (not yet done)

1. **Phase 2 — tuning + hardening.** Admission-policy tuning (caps, redirect vs refuse),
   cleanup-timeout / forced-failure tuning, `qp_num`-reuse stress, and the full observability set
   (§12). The review-driven correctness gaps are enumerated in the §13 TODO.
2. **Phase 3 (deferred) — bounded-spin idle hybrid.** Idle-CPU fallback (§5.4).
3. **Phase 4 — scale-out.** Least-loaded sharding, SMT-aware placement, NUMA-aware Context/PD, and
   extending the same driver to send-recv / credit-ring.

### Teardown as a kernel-like background reclaim

**The mental model is TCP `close()`.** Closing returns immediately; the kernel keeps protocol state
alive in the background (FIN/ACK, retransmit, `TIME_WAIT`) and frees the TCB only once quiescent.
Busy-poll RDMA teardown has the same shape:

| TCP | Busy-poll read-ring |
|---|---|
| `close()` returns immediately | transport `Drop` returns immediately (never blocks a worker) |
| kernel background finishes FIN/ACK, drains in-flight segments | the **`CoreDriver`** reaps the QP's flush CQEs off the shared CQ |
| free the TCB when quiescent | destroy MW→QP→MR→PD once **both WR counters hit zero** |
| `TIME_WAIT` blocks 4-tuple reuse | **`qp_num` retirement** blocks routing-key reuse until drained |
| kernel worker/soft-IRQ context (not the app) | the per-core driver task (not the app task) owns cleanup |

Reclaim runs **per-core** — the `CoreDriver` already on the connection's owning core *is* the
background reaper (a global reclaimer would touch other cores' non-thread-safe ibverbs objects and
reintroduce cross-core contention), and a connection's QP/MRs/ring/`ConnSlot` are core-affine for
life. The **process-level shutdown join** fans out to every core's driver. The protocol:

- **Handoff (never blocks).** `Drop` marks the slot `Closing`, forces the QP to `ERR` (`to_error()`,
  so every outstanding WR flushes a CQE up front and the drain terminates), and `take()`s the
  RDMA/CM resources onto the driver's reclaim queue — RAII destroys nothing, and `Drop` calls no
  owner-checked method, so it is sound even if dropped from a foreign thread.
- **Drain (the barrier).** Each turn the driver clears the slot's inbox backlog and `dec_posted`s
  (but discards) further flush CQEs for a `Closing` slot; when both WR counters are zero and both
  inboxes empty the bundle is `Drained`.
- **Free + retire (the `TIME_WAIT`).** On `Drained` the driver frees MW→QP→MR→PD in field order,
  then retires the slot — bump generation, remove `qp_num` from the routing map — so a straggler
  flush CQE can never be mis-routed to a reused `qp_num` (§4.2).
- **Shutdown join (`main` waits).** `shutdown()` flips a flag; the driver keeps sweeping +
  reclaiming until the queue is empty, and the owner awaits the driver task **before** the shared
  CQs and per-core context drop — so no QP/MR outlives, and no flush CQE is stranded in, a CQ about
  to be destroyed.

**Wedge escape hatch.** A wedged NIC can fail to deliver a flush CQE, so reclaim force-frees a
bundle after a bounded budget and logs it — trading a small logged risk for a shutdown that cannot
hang (the same choice the kernel makes with `TIME_WAIT`/`FIN_WAIT` timeouts). The current
turn-counter budget and its interaction with the retirement `debug_assert` are called out in the
§13 TODO. The concrete `process_reclaim` loop and the `ReadRingInner` split live in
[`core_driver.rs`](../../rdma-io/src/core_driver.rs) and
[`read_ring_transport.rs`](../../rdma-io/src/read_ring_transport.rs).

### Multi-connection: sharding, server accept, and CQ budgeting

Busy-poll is multi-connection and benchmarkable via a per-core pool. The design decisions that
shaped the code (`rdma-io-busy`):

- **Runtime ownership stays with the caller.** The `rdma-io` core depends on tokio with only `net`
  (no `rt`); `CoreDriver::run()` is a plain `async fn` the caller spawns. Runtime topology (which
  cores to pin, NUMA/SMT layout, panic handlers) is deployment policy, so the pool lives in the
  harness-layer `rdma-io-busy` crate, not the core. What *does* belong in the core is the
  placement/admission arithmetic (`ReadRingConfig::wr_budget`).
- **Co-location is mandatory.** The `ConnSlot` inbox is same-core SPSC and the owner-token guard
  (§7.5) panics off-core, so the transport is core-affine for life. The pool's connect/accept are
  therefore **combinators that run the app closure on-core** (`spawn_connect`, `serve`), never
  handing the (`Send`) transport back to the caller — keeping the safe path the only path. Routing
  application data *across* cores is the one case that needs an extra cross-core channel; avoiding
  it is the whole point of thread-per-core, so placement is the caller's to own. The data *bytes*
  are untouched (`send_copy`'s single copy into the send ring, `recv_buf`'s in-place slice); the
  only added handoff is the same-core copy of each one-cache-line `WorkCompletion` into the inbox —
  the intrinsic sole-reaper tax of a shared CQ, present with one connection too.
- **Placement.** Round-robin among cores with headroom, core-affine for life (no migration). A
  least-loaded hook is future work.
- **Server accept (serialized handshake).** The listener has a single CM event channel, so the
  shipped `BusyPool::serve` serializes setup (one handshake touching the listener at a time) to
  avoid mis-binding an `Established` to the wrong accept. The full concurrent CM-ID-keyed routing
  state machine (§6.1, server side) stays a future optimization for accept-heavy churn.
- **Aggregate CQ sizing + admission + flush headroom (§7.2).** Each core's shared CQs are sized
  `(conns_per_core + 1) * per_conn_wrs` (the `+1` is one reclaiming connection's flush headroom),
  validated against `max_cqe`; admission is capped per core. Client connect refuses when all cores
  are full; the server currently over-admits on the round-robin core (server-side reject/redirect is
  in the §13 TODO).
- **Bench integration.** `--mode echo-busy` / `rh1-busy` / `rh1-park` select the busy topology (N
  pinned `current_thread` cores) in the same `rdma-bench-{client,server}` executables; a single
  `--threads` knob is the per-process CPU budget in both modes. Results (`throughput_rps`, p50/p99,
  `cpu_us_per_op`) are recorded under `docs/bench/` with the reboot-between methodology. Busy-poll
  trades CPU for latency, so `cpu_us_per_op` is the honest cost axis (a pinned core runs ~100% under
  load).

## 11. Risks and open questions

- **CPU cost.** Pure spin burns a core per poller; viable only when latency justifies dedicating
  cores. The deferred hybrid trades that for spin↔park tuning.
- **Connection imbalance.** Static sharding can hot-spot a core; least-loaded placement helps but a
  long-lived heavy connection cannot be migrated cheaply (its QP/CQ/slot are core-affine).
- **Admission vs utilization.** Conservative CQ sizing caps connections per core; too tight wastes
  the core, too loose risks overrun. Needs empirical tuning per NIC (`max_cqe`, doorbell counts).
- **Benchmark harness shape.** Busy-poll is N pinned `current_thread` runtimes with sharding, a
  different runtime topology from the multi-thread `Send` harness. Rather than a separate binary it
  is selected by `--mode echo-busy` in the **same** `rdma-bench-{client,server}` executables, whose
  `main` branches on the mode to build the busy runtime topology before any arm-park runtime (§10).
- **Platform.** Read-ring only, RoCE/IB only (Write+Imm; no iWARP/siw). Extending to other
  transports is Phase 4.

## 12. Validation and observability

**Correctness tests (run on rxe and MANA — completion timing and teardown differ):** exactly-once
routing of data, padding, and Read-sentinel completions; completion arriving immediately
before/during/after waker registration; simultaneous read+write waiters; multiple completions for
one connection in a batch; mixed hot/idle connections on one CQ; one connection flooding
completions (fairness); cancel with outstanding WRs; disconnect/flush while unrelated QPs stay
active; rapid close/reconnect with `qp_num` reuse; shared-CQ and inbox saturation; setup failure at
each construction stage; driver shutdown and driver-task panic; timer/`AsyncFd` progress under a
continuously-ready driver; and **regression of all arm-park transports and constructors**.

> Implemented so far in `read_ring_busy_pool_tests` (MANA): multi-connection routing, hot/idle
> fairness, per-connection disconnect isolation, admission-cap enforcement, and shutdown-join
> reclaim. The remaining fault-injection cases (rxe runs, waker races, `qp_num`-reuse churn, and the
> review-driven scenarios) are tracked in §13.

**Metrics:** total vs empty CQ polls; CQEs per batch and per turn; **poll-gap histogram + max**;
per-connection inbox depth + high-water; useful/coalesced/redundant wakes; unknown/stale-QP
completions; CQ errors + overrun; outstanding WRs at close + close duration; CM event count +
disconnect-detection latency; reactor-turn cadence + syscall rate; per-core CPU/affinity/NUMA/SMT
placement; per-core connection count.

## 13. Remaining work and review-driven hardening TODO

The feature is implemented and experimentally validated, but not yet lifecycle-complete for
arbitrary failure, cancellation, and shutdown. The items below are from the 2026-07-15
implementation review, in fix order.

> **Progress (2026-07-15).** The three "must-fix-now" items are done: **8** (readiness barrier),
> **6** (forced-reclaim panic — the panic; the monotonic-deadline refinement is still open), and
> **13** (argument/result hygiene). See the per-item **[Done]** notes below.

**Blockers (before production):**

1. **Setup-failure cleanup.** A busy connect/accept that fails after slot registration drops the
   half-built QP/MRs locally and leaves a registered slot. Add a setup RAII guard that marks the
   slot `Closing`, forces the QP to `ERR`, hands the partial bundle to the driver reclaim queue, and
   keeps the slot registered until retirement. Make `CoreDriverHandle::register()` reject an existing
   `qp_num` instead of replacing it.
2. **Admission vs retirement accounting.** `AdmissionGuard` releases a slot when the app task ends,
   but the old QP may still have a full budget of flush CQEs pending, so `~2×cap` connections can
   share a CQ sized for `cap+1`. Move the admission lease into the reclaim entry and release it only
   after retirement; give server over-admission a strict bound reflected in CQ sizing (reject or
   redirect when full).
3. **Shutdown ordering.** Pool shutdown can stop a driver before active connection tasks drop,
   stranding late reclaim entries. Signal app loops, await (or abort-then-await) every task so each
   transport `Drop` enqueues reclaim, then stop the driver — and refuse driver exit while any
   non-retired slot is still registered.

**High priority:**

4. **Terminal-error propagation.** Inbox overflow / CQ-poll failure / unknown-`qp_num` are logged
   but not surfaced: `poll_inbox()` never checks `is_fatal()`, so a connection can hang instead of
   erroring. Store a concrete terminal error in `ConnSlot`, return it from `poll_inbox()`, wake both
   directions, and drive QP→`ERR`. A CQ-poll failure should fail every slot on that core.
5. **Mandatory affinity + release-mode owner check.** Worker pinning ignores
   `core_affinity::set_for_current()` and runners pick `0..N` rather than the allowed CPU set; the
   owner guard is only `debug_assert` and runs *after* the verbs post. Enumerate allowed cores, fail
   worker creation if pinning fails, and check the owner token (release builds too) *before* every
   busy QP/CM op.
6. **Forced-reclaim deadline (fixes a debug panic).** `process_reclaim`'s wedge path calls
   `ConnSlot::retire()`, whose `debug_assert!(drain_complete())` fires exactly when the escape hatch
   is exercised. Separate forced abandonment from normal `retire()`, use a monotonic deadline +
   progress tracking instead of a turn count, and quarantine the `qp_num` (or fail the driver) rather
   than claim the reuse barrier held.
   **[Done 2026-07-15 — panic + quarantine]** Added `ConnSlot::abandon()` (no drain assertion) and
   split `process_reclaim`: the normal drain retires + frees the `qp_num` for reuse; the wedge path
   `abandon()`s and **quarantines** the number (leaves the `Drained` slot registered so it is never
   reused and stragglers are counted-down + discarded). Unit test
   `abandon_bumps_generation_without_drain_barrier`. **Still open:** replacing the turn-count budget
   with a monotonic deadline + progress tracking.
7. **Setup cancellation + timeouts.** `BusyPool::serve` waits unconditionally for each setup ack, so
   the server can't observe SIGTERM until it has accepted the configured count; token acquisition
   ignores `token_timeout`. Select shutdown against waiting for a request and each ack, and apply
   `token_timeout` to token/setup waits (timeouts enter the setup reclaim guard, item 1).
8. **Benchmark readiness barrier.** Busy clients arm warmup/bench deadlines before connect + protocol
   setup, biasing busy-vs-park comparisons. Add a barrier: every on-core task finishes setup, reports
   ready, and waits for a shared start signal; start sampling/deadlines only when all requested
   connections are ready; mark the run invalid otherwise.
   **[Done 2026-07-15]** `run_readiness_barrier` (+ `ConnReady` / `StartGate` / `BarrierOutcome`) in
   the bench `metrics` module: each connection reports ready after setup and blocks on a shared start
   gate; the orchestrator starts the deadlines and the CPU/RSS sampler only once every connection is
   accounted for, then releases the gate. All four thread-per-core clients (`echo-busy`/`echo-park`/
   `rh1-busy`/`rh1-park`) use it; incomplete readiness prints `RUN INVALID`.

**Medium priority:**

9. **Remove/guard public `deregister()`.** `CoreDriverHandle::deregister()` removes a slot with no
   counter/lifetime check, contradicting the retirement invariant. Remove it or gate it behind a
   completed-retirement proof.
10. **Defensive sizing/construction validation.** Reject empty core sets and zero caps, use checked
    arithmetic/casts for CQ depths, and require a successful device capability query (don't treat a
    failed `wr_budget` device query as unbounded).
11. **Lifecycle fault-injection tests.** Setup failure/cancellation at each stage; simultaneous
    close + re-admit at cap; shutdown with active tasks; token/setup timeout; inbox-overflow and
    CQ-poll propagation; unknown-`qp_num` terminal behavior; forced-reclaim timeout; off-core misuse
    in release; empty/invalid core sets; explicit proof of `qp_num` reuse/quarantine; rxe runs and
    the waker races (§12).
12. **Observability.** Add per-driver poll/batch/gap metrics, per-slot inbox high-water + terminal
    state, reclaim duration/outcome counters, and verified CPU/NUMA identity; emit a compact per-core
    summary in the benchmark JSON.
13. **Argument/result hygiene.** Reject an incompatible `--transport` for busy modes instead of
    silently coercing to read-ring; count busy task panics / setup failures as errors; mark a run
    invalid if fewer than the requested connections reach the readiness barrier.
    **[Done 2026-07-15]** `common::require_read_ring` hard-errors the four thread-per-core modes on
    any non-`read-ring` transport (was a silent note that mislabeled the JSON + filename); the
    readiness barrier (item 8) counts panics / setup failures / admission-refusals as errors and
    prints `RUN INVALID` when fewer than requested connections reach the barrier.

## 14. Where to go next

- [rdma-read-ring-transport.md](rdma-read-ring-transport.md) — the data path both modes reuse.
- [../background/RdmaMechanisms.md](../background/RdmaMechanisms.md) — busy-poll vs arm-and-park,
  why a poll-only CQ needs no fd, completion→data ordering.
- [`async_cq.rs`](../../rdma-io/src/async_cq.rs) — the arm-then-drain code `ArmParkSource` wraps and
  the deferred hybrid reuses.
- [read-ring-concurrent-stream-deadlock.md](../bugs/read-ring-concurrent-stream-deadlock.md) —
  the single-owner / liveness invariants both modes preserve.
- [../future/RingPerformance.md](../future/RingPerformance.md) — broader ring perf roadmap.

## Appendix A — the `CompletionSource` seam (design decisions)

The seam is implemented in [`completion_source.rs`](../../rdma-io/src/completion_source.rs) (an enum
with `ArmPark` and `Driver` variants) and the poster QP in
[`async_qp.rs`](../../rdma-io/src/async_qp.rs); the code is the reference for exact signatures. The
design decisions it encodes, for future readers:

1. **`CqPollState` lives inside `ArmParkSource`.** The drain-after-arm state moves off the transport
   into the source that owns the CQ, so `poll_completions(cx, out)` carries no external state.
2. **CQ ownership + drop order.** The QP holds a non-owning pointer to its CQ, so the transport
   orders `qp` **before** `send_src`/`recv_src` in its fields; RAII drops the QP first and the
   arm-park CQs after, preserving the verbs "destroy QP before its CQ" rule. In busy-poll the shared
   CQ is driver-owned and destroyed only at shutdown, after the teardown barrier (§6.2) reclaims
   every bundle — same invariant, enforced by the barrier instead of field order.
3. **Setup before the transport exists.** Build the source(s) first, drain setup WRs through them
   (`try_drain` / the slot inbox), then move them into the constructed transport — nothing else pokes
   a CQ. `drain_setup` replaced `transport_common::drain_send_cq`.
4. **Static dispatch, exactly two variants.** An enum (not `dyn`) keeps dispatch static; the
   read-ring interpretation (Read-sentinel, imm decode, `CompletionTracker`) stays in the transport
   for both modes. `AsyncQp` is slimmed to a poster (`post_*`, `qp_num`, `to_error()`) with its raw
   CQ accessors removed.
