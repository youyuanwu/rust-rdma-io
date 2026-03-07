# Demikernel — RDMA Support Analysis

**Repository:** <https://github.com/microsoft/demikernel/tree/dev>
**Paper:** [SOSP '21 — The Demikernel Datapath OS Architecture for Microsecond-scale Datacenter Systems](https://doi.org/10.1145/3477132.3483569)
**Reviewed:** 2026-03-06

## What Is Demikernel?

Demikernel is a **library operating system (LibOS) architecture** from Microsoft Research,
designed for kernel-bypass I/O devices. It provides a uniform system call API (called
**PDPIX** — a queue-based extension of POSIX) across multiple kernel-bypass technologies.
Applications interact through queue descriptors (QDs) and asynchronous `push`/`pop`
operations rather than traditional file descriptors.

Written in Rust. Published at SOSP 2021.

## Current LibOS Implementations (dev branch)

| LibOS        | Transport                  | Cargo Feature       | Active? |
|--------------|----------------------------|---------------------|---------|
| **catnap**   | Linux Sockets / Winsock    | `catnap-libos`      | ✅ Yes  |
| **catnip**   | DPDK                       | `catnip-libos`      | ✅ Yes  |
| **catpowder**| Linux Raw Sockets / XDP    | `catpowder-libos`   | ✅ Yes  |

These three are the only `LibOSName` variants in `src/demikernel/libos/name.rs`.

## RDMA Support — History and Status

### The SOSP Paper (2021)

The SOSP '21 paper described an RDMA LibOS as one of the key Demikernel backends. The paper
showed it offloading the TCP/IP stack to the RDMA NIC and evaluated it with Redis and
TAPIR (TxnStore). However, the paper never gave this RDMA LibOS a "cat-name" — that naming
came later in the codebase.

### The C++ RDMA LibOS (v0.0 — Legacy)

The original C++ Demikernel (tagged `v0.0`, Sep 2021) had a **real RDMA LibOS** at
`src/c++/libos/rdma/`. Source: [`rdma_queue.cc`](https://github.com/microsoft/demikernel/blob/v0.0/src/c%2B%2B/libos/rdma/rdma_queue.cc).

**Architecture:**
- **Connection management:** Used `rdma_cm` (`rdma_resolve_addr`, `rdma_connect`,
  `rdma_accept`, `rdma_listen`). Non-blocking event channel polling for CM events.
- **Queue pairs:** `IBV_QPT_RC` (Reliable Connection) with separate send/recv CQ
  sharing a single `ibv_cq`.
- **Data transfer:** Two-sided RDMA operations — `ibv_post_send` with `IBV_WR_SEND`,
  `ibv_post_recv` for receives.
- **Message-based, NOT byte-stream:** Each `push()` sends a `dmtr_sgarray_t` (scatter-gather
  array) as a single RDMA message with a header (`magic`, `total_len`, `num_segments`,
  per-segment lengths). Each `pop()` receives a complete message. No framing, reassembly,
  or TCP-like byte-stream semantics.
- **Flow control via RDMA WRITE:** The sender tracks a `send_window` of available recv
  buffers on the remote side. The receiver uses `IBV_WR_RDMA_WRITE` to update the sender's
  window counter (`their_send_window_addr`/`their_send_window_rkey`) when it posts new recv
  buffers. This is a one-sided write to a registered memory region.
- **Receive window:** Pre-posts `RECV_WINDOW * RECV_WINDOW_BATCHES` recv buffers. When the
  window drops below threshold, allocates a new batch and notifies the sender via RDMA WRITE.
- **Memory:** Custom Hoard-RDMA allocator (`HoardRdma`) that registers all heap memory for
  RDMA access. Zero-copy via `ibv_mr` registration. Inline sends for messages < 128 bytes.
- **Send signaling:** Not every send is signaled — uses `SEND_SIGNAL_FREQ` to reduce CQ
  overhead (signals every Nth send).
- **Coroutine scheduling:** push/pop/accept/connect each run in cooperative coroutine
  threads that yield when blocked (e.g., waiting for recv window, waiting for CM events).

**Companion applications:** `redis-rdma`, `tapir-rdma` (modified Redis and TAPIR to use
the Demikernel RDMA LibOS).

### catcollar (Rust, v1.0–v1.3) — NOT RDMA

Despite the SOSP paper's RDMA focus, `catcollar` in the Rust rewrite was actually an
**io_uring-based socket LibOS**, not an RDMA LibOS:

- Created regular Linux TCP/UDP sockets via `libc::socket()`
- Submitted send/recv via io_uring (`io_uring_prep_sendmsg` / `io_uring_prep_recvmsg`)
- Dependencies: `liburing` (not libibverbs)
- Essentially "sockets accelerated by io_uring" — still kernel-mediated, no RDMA
- Source: `src/rust/catcollar/` at tag `v1.3`

The RDMA LibOS from the C++ era was **never ported to the Rust codebase**.

### catcollar Removed in [PR #1151](https://github.com/microsoft/demikernel/pull/1151)

catcollar (io_uring) and catloop (TCP loopback) were both removed. As of `dev` branch
(v1.5.13):
- No `catcollar` code, no RDMA feature, no ibverbs/rdma_cm dependencies
- `LibOSName` enum has only: `Catpowder`, `Catnap`, `Catnip`

### Demikernel Org — Related RDMA Repos

The [demikernel GitHub org](https://github.com/orgs/demikernel/repositories) has two
experimental RDMA repos, both stale:

| Repo | Description | Last Updated |
|------|-------------|--------------|
| [`demikernel/rdma-cm`](https://github.com/demikernel/rdma-cm) | Idiomatic Rust bindings for RDMA CM + ibverbs. Zero-cost wrappers. | Feb 2023 |
| [`demikernel/io-queue-rdma`](https://github.com/demikernel/io-queue-rdma) | Experimental io-queue abstraction over RDMA in Rust | Dec 2021 |

The `demikernel/rdma-cm` crate is architecturally very similar to our `rdma-io` — it wraps
`ibv_cq`, `rdma_cm_id`, etc. in safe Rust types. Its README explicitly discusses why they
built their own instead of using `rust-ibverbs` (same reasons we had: ibverbs crate doesn't
support RDMA CM).

## Does Demikernel Implement a Byte Stream on Top of RDMA?

**No.** At no point in Demikernel's history was there a byte-stream abstraction over RDMA:

1. **The C++ RDMA LibOS (v0.0)** was **message-based**. The PDPIX API uses `push(sga)`/`pop()`
   where each push sends a complete scatter-gather message and each pop receives one. There
   is a message header with magic/length/segment-count, but no byte-stream framing,
   reassembly, or partial-read semantics. This is fundamentally different from TCP's byte
   stream — it's more like UDP datagrams over reliable RDMA.

2. **catcollar (Rust)** wasn't RDMA at all — it was io_uring over sockets, which inherits
   the kernel TCP byte-stream.

3. **The PDPIX API itself** is designed around messages (scatter-gather arrays), not byte
   streams. The `push`/`pop` model maps naturally to RDMA send/recv semantics.

This is a key **architectural difference from rust-rdma-io**, which implements
`AsyncRead`/`AsyncWrite` byte-stream traits on top of RDMA send/recv, providing TCP-like
semantics that integrate with tokio and tonic/gRPC.

## Architecture Comparison

| Aspect                | Demikernel RDMA (C++ v0.0)            | rust-rdma-io                                 |
|-----------------------|---------------------------------------|----------------------------------------------|
| **Language**          | C++                                   | Rust + C (inline fn wrappers)                |
| **RDMA operations**   | Two-sided send/recv                   | Two-sided send/recv                          |
| **QP type**           | RC (Reliable Connection)              | RC (Reliable Connection)                     |
| **Connection mgmt**   | rdma_cm                               | rdma_cm                                      |
| **Data model**        | Message-based (scatter-gather arrays) | **Byte stream** (AsyncRead/AsyncWrite)       |
| **Flow control**      | RDMA WRITE to remote window counter   | rnr_retry (hardware) + pre-posted recv bufs  |
| **CQ model**          | Single shared CQ, polled by coroutine | Separate send_cq/recv_cq, async (tokio)      |
| **Memory**            | Custom Hoard-RDMA allocator           | User-managed buffers, MR registration        |
| **API**               | PDPIX push/pop (custom syscall)       | Standard Rust async traits, tonic-compatible |
| **Scheduling**        | Cooperative coroutines (custom)       | tokio async runtime                          |
| **Abstraction level** | Full LibOS replacement                | Thin safe wrappers over verbs/CM             |

## Key Takeaways

1. **Demikernel had real RDMA support only in the C++ era (v0.0).** The Rust rewrite never
   included an RDMA LibOS. catcollar was io_uring, not RDMA.

2. **No byte stream.** Demikernel's RDMA was message-based (push/pop scatter-gather arrays),
   matching RDMA's native message semantics. rust-rdma-io's byte-stream approach (for
   TCP-drop-in compatibility with tonic/gRPC) is a fundamentally different design choice.

3. **Flow control approach differs.** Demikernel used RDMA WRITE to a remote memory region
   to update the sender's view of available recv buffers. rust-rdma-io relies on hardware
   RNR retry (`rnr_retry_count=7`) plus pre-posted recv buffers.

4. **The `demikernel/rdma-cm` crate** is the closest thing to our approach — safe Rust
   wrappers around rdma_cm + ibverbs. It's stale (2023) but validates the design direction.

5. **No code reuse opportunity.** The C++ code is architecturally different (message-based,
   custom allocator, cooperative scheduling). The Rust experimental repos are stale and
   incomplete.
