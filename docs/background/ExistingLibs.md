# Existing Rust RDMA Libraries — Detailed Analysis

> Surveyed: Feb 2026. Stars/activity as of survey date.

---

## 1. rust-ibverbs (`ibverbs` crate)

| | |
|---|---|
| **Repo** | [jonhoo/rust-ibverbs](https://github.com/jonhoo/rust-ibverbs) |
| **Stars** | 203 |
| **License** | MIT / Apache-2.0 |
| **crates.io** | [`ibverbs`](https://crates.io/crates/ibverbs) (v0.9.2) |
| **Last commit** | Feb 2026 (Bazel build support) |
| **Author** | Jon Gjengset (well-known Rust educator / contributor) |

### What it provides

- **Safe Rust wrapper** around the C `libibverbs` library
- Covers the core ibverbs control path: Device, Context, ProtectionDomain, CompletionQueue, QueuePair, MemoryRegion, AddressHandle
- **Data path**: post_send, post_receive, poll_cq
- QP types: RC (Reliable Connected) — the most common type
- Thread-safe (`Sync + Send`) for all types
- **Serde support** for serializable types (optional feature)
- Builds against vendored `rdma-core` via git submodule, or system `libibverbs`
- Bazel build support (recently added)

### Architecture

- **Two crates**: `ibverbs-sys` (FFI bindings, auto-generated via bindgen + cmake) and `ibverbs` (safe wrapper)
- Single `lib.rs` (~80KB) — monolithic, all types in one file
- Uses RAII for resource cleanup (Drop impls on all types)
- Builder pattern for QP creation

### Strengths

- Well-documented (extensive doc comments, references to RDMAmojo)
- Clean idiomatic Rust API
- Dual-licensed (permissive)
- Mature — created 2017, steady maintenance
- Good starting point for learning RDMA in Rust

### Weaknesses

- **No rdmacm support** — connection management must be done manually (exchange QP info out-of-band)
- **No async support** — blocking CQ polling only
- **Limited QP types** — primarily RC, no UD/UC/XRC/DC
- **No SRQ (Shared Receive Queue)** support
- No new ibverbs API (`ibv_wr_*`, `ibv_start_poll`)
- Monolithic single-file design doesn't scale well

---

## 2. async-rdma

| | |
|---|---|
| **Repo** | [datenlord/async-rdma](https://github.com/datenlord/async-rdma) |
| **Stars** | 429 (highest among Rust RDMA libs) |
| **License** | GPL-3.0 |
| **crates.io** | [`async-rdma`](https://crates.io/crates/async-rdma) (v0.5.0) |
| **Last commit** | Nov 2023 (inactive ~2+ years) |

### What it provides

- **High-level async framework** for RDMA applications
- Built on `rdma-sys` (raw FFI bindings) and `tokio`
- Connection establishment via `RdmaBuilder` (listen/connect model)
- High-level operations: `read`, `write`, `send`, `receive`
- **Memory region management**: `alloc_local_mr`, `request_remote_mr`, `send_mr`, `receive_local_mr`
- Background agent thread for MR management and request execution
- CQ event-driven completion (async, not polling)
- Memory region allocator with jemalloc integration

### Architecture

- Depends on [`rdma-sys`](https://github.com/datenlord/rdma-sys) for FFI bindings
- Modules: `agent`, `completion_queue`, `context`, `cq_event_listener`, `device`, `memory_region`, `mr_allocator`, `protection_domain`, `queue_pair`, `rmr_manager`, `work_request`
- Agent-based architecture: background thread handles RDMA operations
- Event-driven CQ completion via `cq_event_listener`
- Builder pattern (`RdmaBuilder`) for connection setup

### Strengths

- **Highest-level API** among all Rust RDMA libs — easy to get started
- **Async/await** support with tokio integration
- Sophisticated MR management (allocation, remote MR exchange)
- Good examples and environment setup guide
- Most starred Rust RDMA project

### Weaknesses

- **GPL-3.0 license** — restrictive for many use cases
- **Inactive** — last commit Nov 2023, 22 open issues
- Heavy dependency footprint (tokio, jemalloc, bincode, async-bincode, etc.)
- Opinionated architecture (agent-based) may not suit all use cases
- Only works with tokio (not runtime-agnostic)
- Pinned tokio version (`=1.29.1`)
- No new ibverbs API support

---

## 3. rdma (Nugine)

| | |
|---|---|
| **Repo** | [Nugine/rdma](https://github.com/Nugine/rdma) |
| **Stars** | 39 |
| **License** | MIT |
| **crates.io** | [`rdma`](https://crates.io/crates/rdma) (v0.4.0-dev) |
| **Last commit** | Oct 2023 (inactive ~2+ years) |

### What it provides

- **Low-level RDMA API** — thin safe wrapper staying close to the C API
- Covers ibverbs resources: Context, PD, CQ, QP, MR, MW, AH, SRQ, CompletionChannel, DeviceMemory
- Work requests and work completions
- Reference-counted resource management
- Inline FFI bindings generated via bindgen at build time
- RC and UD service types supported (with pingpong examples)
- Async RPC example

### Architecture

- Monorepo with workspace: single `rdma` crate under `crates/rdma`
- Well-organized modules: `ah`, `cc`, `cq`, `ctx`, `device`, `dm`, `error`, `mr`, `mw`, `pd`, `qp`, `srq`, `wc`, `wr`
- Resource graph tracked via reference counting + `weakset`
- Build-time bindgen against system `libibverbs` and `librdmacm` (via `pkg-config`)
- Uses `bitflags` for flag types

### Strengths

- **Clean modular design** — well-separated concerns
- MIT licensed (permissive)
- Covers more resource types than `ibverbs` (SRQ, MW, DM, AH)
- RC and UD support
- Resource dependency graph explicitly tracked
- Good code quality

### Weaknesses

- **Inactive** — last commit Oct 2023
- Still in dev (0.4.0-dev, not stable)
- Memory management APIs are all unsafe (by design — still exploring safe abstractions)
- Limited documentation
- No rdmacm connection management
- No async runtime integration (though has async example)

---

## 4. sideway

| | |
|---|---|
| **Repo** | [RDMA-Rust/sideway](https://github.com/RDMA-Rust/sideway) |
| **Stars** | 77 |
| **License** | MPL-2.0 |
| **crates.io** | [`sideway`](https://crates.io/crates/sideway) (v0.4.0) |
| **Last commit** | Feb 2026 (actively maintained!) |

### What it provides

- **Modern Rust wrapper** for RDMA programming APIs
- Focuses on **new ibverbs API** (`ibv_wr_*`, `ibv_start_poll`, etc.) — the newer, more performant API surface
- Covers both `ibverbs` and `rdmacm` modules
- Built on [`rdma-mummy-sys`](https://github.com/RDMA-Rust/rdma-mummy-sys) — a custom FFI crate that can compile without rdma-core headers (uses a "mummy" approach to handle inline functions)
- Does NOT rely on rdma-core at compile time (only at runtime)

### Architecture

- Source organized into `src/ibverbs/` and `src/rdmacm/` modules
- Dependencies: `rdma-mummy-sys`, `libc`, `thiserror`, `serde`, `bitmask-enum`, `tabled`
- Lightweight dependency footprint
- Property-based testing with `proptest`
- Good test infrastructure (`rstest`, `trybuild`)

### Strengths

- **Most actively maintained** Rust RDMA library (commits in Feb 2026)
- **New ibverbs API** support — forward-looking design
- **rdmacm support** — connection management included
- **No compile-time rdma-core dependency** — easier cross-compilation and CI
- Clean separation of ibverbs and rdmacm
- Good testing practices (property tests, compile-fail tests)
- Reasonable license (MPL-2.0)

### Weaknesses

- Still relatively new (created Aug 2024)
- 25 open issues — actively evolving, API not fully stable
- Traditional ibverbs API (`ibv_post_send`, `ibv_poll_cq`) wrapped but "no performance guarantee"
- No async support
- Smaller community than async-rdma or rust-ibverbs

---

## 5. rrddmma

| | |
|---|---|
| **Repo** | [IcicleF/rrddmma](https://github.com/IcicleF/rrddmma) |
| **Stars** | 13 |
| **License** | MIT |
| **crates.io** | [`rrddmma`](https://crates.io/crates/rrddmma) (v0.7.3) |
| **Last commit** | Apr 2025 |

### What it provides

- Rust RDMA library **specialized for Mellanox/NVIDIA ConnectX** NICs
- Supports multiple linkage types: MLNX_OFED v4.9-x, MLNX_OFED v5.x, system `libibverbs`, or build rdma-core from source
- RC and UD QP types
- **Dynamically Connected QPs (DC)** — via MLNX_OFED experimental APIs
- **Extended Atomics** — via MLNX_OFED experimental APIs
- Hi-level and Lo-level API layers
- Distinct error handling philosophy: panics for programming errors, `Result` for runtime errors

### Architecture

- Source: `src/bindings/` (FFI), `src/ctrl/` (control path), `src/hi/` (high-level), `src/lo/` (low-level), `src/utils/`
- Layered design: bindings → lo → hi
- Build system handles MLNX_OFED detection and fallback
- Can vendor and statically link rdma-core

### Strengths

- **Mellanox-specific features** (DC QPs, extended atomics) not available elsewhere
- Layered API (choose your abstraction level)
- Good error handling design philosophy
- MIT licensed
- Published on crates.io with regular releases

### Weaknesses

- **Academic-oriented** — explicitly stated "more for academic use"
- **Unstable interfaces** — "under continuous change"
- Specialized to Mellanox hardware
- Small community
- No rdmacm support
- No async support

---

## 6. rdma-core (lemonrock)

| | |
|---|---|
| **Repo** | [lemonrock/rdma-core](https://github.com/lemonrock/rdma-core) |
| **Stars** | 18 |
| **License** | MIT |
| **Last commit** | Apr 2018 (abandoned) |

### What it provides

- Low-level FFI binding of `libibverbs` (rdma-core-sys crate)
- Incomplete mid-level binding (rdma-core crate)
- GASPI 2 and OpenSHMEM 1.3 specification header bindings (non-linking)

### Verdict

**Abandoned**. Last commit 2018. Uses outdated Rust patterns (requires nightly). Non-standard naming conventions. Not usable for any modern project. Historical interest only.

---

## 7. rdma-sys (datenlord)

| | |
|---|---|
| **Repo** | [datenlord/rdma-sys](https://github.com/datenlord/rdma-sys) |
| **Stars** | 49 |
| **crates.io** | [`rdma-sys`](https://crates.io/crates/rdma-sys) (v0.3.0) |

Raw FFI bindings to `libibverbs` and `librdmacm`. Used by `async-rdma`. Handles inline functions manually. Published but not actively developed.

---

## 8. rdma-rs (phoenix-dataplane)

| | |
|---|---|
| **Repo** | [phoenix-dataplane/rdma-rs](https://github.com/phoenix-dataplane/rdma-rs) |
| **Stars** | 4 |
| **Last commit** | 2024 |

Fork/derivative of `rust-ibverbs` that adds `rdmacm` support. Incomplete (TODO: docs, examples, remove assertions). Developed for the Phoenix dataplane research project.

---

## Comparative Summary

| Feature | rust-ibverbs | async-rdma | rdma (Nugine) | sideway | rrddmma |
|---|---|---|---|---|---|
| **Stars** | 203 | 429 | 39 | 77 | 13 |
| **Active** | ✅ (Feb 2026) | ❌ (Nov 2023) | ❌ (Oct 2023) | ✅ (Feb 2026) | ⚠️ (Apr 2025) |
| **License** | MIT/Apache-2.0 | GPL-3.0 | MIT | MPL-2.0 | MIT |
| **Abstraction** | Mid-level | High-level | Low-level | Mid-level | Layered |
| **Async** | ❌ | ✅ (tokio) | ❌ | ❌ | ❌ |
| **rdmacm** | ❌ | ✅ (via rdma-sys) | ❌ | ✅ | ❌ |
| **New ibverbs API** | ❌ | ❌ | ❌ | ✅ | ❌ |
| **QP types** | RC | RC | RC, UD | RC, UD, + | RC, UD, DC |
| **SRQ** | ❌ | ❌ | ✅ | ? | ? |
| **MR management** | Basic | Advanced | Unsafe | Basic | Basic |
| **Thread safety** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **serde** | ✅ | ✅ | Optional | ✅ | ✅ |
| **Compile w/o rdma-core** | ❌ | ❌ | ❌ | ✅ | ❌ |
| **crates.io** | ✅ | ✅ | ✅ | ✅ | ✅ |

## Key Gaps Across All Libraries

1. **No library combines async + rdmacm + new ibverbs API** — this is the biggest opportunity
2. **No runtime-agnostic async** — async-rdma is tokio-only; no one supports `smol`, `async-std`, or raw `mio`
3. **io_uring integration** — no library explores io_uring for CQ notification or event loop integration
4. **No `std::io::{Read,Write}` or `tokio::io::{AsyncRead,AsyncWrite}` trait implementations** — RDMA is treated as a separate world from the Rust I/O ecosystem
5. **Memory safety for MR lifecycle** is unsolved — Nugine/rdma acknowledges this explicitly
6. **XRC (Extended Reliable Connected)** support is absent everywhere
7. **Multi-device / multi-port** scenarios poorly handled
8. **No connection pooling or reconnection** abstractions
9. **Observability** (metrics, tracing) is minimal
10. **Documentation** is sparse across all projects except rust-ibverbs
