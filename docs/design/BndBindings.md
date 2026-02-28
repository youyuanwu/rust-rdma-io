# RDMA Bindings via bnd — Design & Implementation

> Using [bnd](https://github.com/youyuanwu/bnd) and [bnd-winmd](https://crates.io/crates/bnd-winmd) to generate Rust FFI bindings for rdma-core.

---

## 1. Overview

`bnd` generates Rust FFI bindings from C headers using an intermediate WinMD (ECMA-335) representation:

```
C Headers ──→ libclang ──→ bnd-winmd ──→ .winmd ──→ windows-bindgen ──→ Rust FFI module
```

For RDMA, we split the work into three partitions in a single generation pass:

1. **`rdma.ibverbs`** — structs, enums, unions, and non-inline function declarations from `verbs.h`
2. **`rdma.rdmacm`** — RDMA Connection Manager types and functions from `rdma_cma.h`
3. **`rdma.wrapper`** — Rust FFI for C wrapper functions that call the 96 `static inline` functions

```
                ┌──────────────────────────────┐
                │        infiniband/verbs.h     │
                │        rdma/rdma_cma.h        │
                │        wrapper/wrapper.h      │
                └──────┬───────────────┬────────┘
                       │               │
              ┌────────▼────────┐ ┌────▼──────────────┐
              │   bnd-winmd     │ │   wrapper.c        │
              │  (3 partitions) │ │  (inline wrappers) │
              └────────┬────────┘ └────┬──────────────┘
                       │               │
              ┌────────▼────────┐ ┌────▼──────────────┐
              │    .winmd       │ │  cc crate compiles │
              └────────┬────────┘ │  → librdma_wrapper │
                       │          └────┬──────────────┘
              ┌────────▼────────┐      │
              │ windows-bindgen │      │
              │  → Rust FFI     │      │
              └────────┬────────┘      │
                       │               │
              ┌────────▼───────────────▼──┐
              │     rdma-io-sys crate     │
              │  (FFI + wrapper library)    │
              └───────────────────────────┘
```

---

## 2. Why bnd Instead of bindgen

| Aspect | bindgen | bnd |
|---|---|---|
| Inline functions | ❌ Skips `static inline` by default | ❌ Not yet supported — but we wrap them in C anyway |
| Output quality | Raw FFI, one giant file | Namespaced modules, idiomatic Rust types |
| Union handling | Generates `__BindgenUnion` types | Cleaner via WinMD type system |
| Enum handling | Integer constants | Proper Rust enums where possible |
| POSIX types | Re-generates from scratch | Imports from `bnd-posix` (shared, consistent) |
| Maintenance | Re-run bindgen on each build | Pre-generate, check into source |
| docs.rs | Needs system headers or fallback | Pre-generated — works out of the box |

The key advantage: bnd produces **pre-generated, checked-in Rust modules** with proper namespacing. The `.winmd` intermediate representation is stable and the generated code doesn't depend on system headers at compile time.

---

## 3. RDMA Header Extraction Results

Generation via `cargo run -p bnd-rdma-gen` against rdma-core 50.0 system headers:

| Partition | Structs | Enums | Functions | Constants |
|---|---|---|---|---|
| `rdma.ibverbs` | 275 | 92 | 164 | 28 |
| `rdma.rdmacm` | 19 | 4 | 40 | 20 |
| `rdma.wrapper` | 0 | 0 | 96 | 0 |
| **Total** | **294** | **96** | **300** | **48** |

### Cross-crate type references

Generated code references types from sibling crates:
- `bnd_posix::posix::socket::sockaddr` — used in `rdma_bind_addr`, `rdma_get_local_addr`
- `bnd_posix::posix::types::ssize_t` — used in `_ibv_query_gid_table`
- `bnd_linux::linux::types::__be16` — used in `ibv_get_pkey_index`, `rdma_get_dst_port`
- `bnd_linux::linux::types::__be64` — used in `ibv_get_device_guid`
- `super::ibverbs::ibv_pd` — rdmacm references ibverbs types via module path

---

## 4. Type Dependencies and WinMD Cross-References

bnd uses a two-stage type reference mechanism:

1. **Generation time**: `[[type_import]]` in the TOML config imports type metadata from an external `.winmd` file into the registry, so bnd-winmd can resolve type names during extraction.
2. **Code generation time**: `windows-bindgen --reference` tells the Rust code generator to emit `bnd_posix::posix::*` paths instead of re-generating posix type definitions locally.
3. **Compile time**: The generated crate declares `bnd-posix` as a Cargo dependency, so `bnd_posix::posix::pthread::pthread_mutex_t` etc. resolve normally.

This is exactly how `bnd-openssl` and `bnd-linux` reference `bnd-posix` types today.

### Already in bnd-posix (direct reference via `--reference bnd_posix,full,posix`)

| Type | bnd-posix module | Used in generated code as |
|---|---|---|
| `pthread_mutex_t`, `pthread_cond_t` | `posix.pthread` | `bnd_posix::posix::pthread::pthread_mutex_t` |
| `uint8_t`, `uint16_t`, `uint32_t`, `uint64_t` | `posix.types` | `bnd_posix::posix::types::uint32_t` |
| `size_t`, `ssize_t` | `posix.types` | `bnd_posix::posix::types::size_t` |
| `sockaddr`, `sockaddr_storage` | `posix.socket` | `bnd_posix::posix::socket::sockaddr` |
| `sockaddr_in`, `sockaddr_in6` | `posix.socket` | `bnd_posix::posix::socket::sockaddr_in6` |

### Missing from bnd-posix — add to bnd-linux directly

The `bnd` repo is available locally at `../bnd` and can be modified directly.

| Type | Header | Notes |
|---|---|---|
| `__be16` | `linux/types.h` | `typedef __u16 __be16` — big-endian annotated u16 |
| `__be32` | `linux/types.h` | `typedef __u32 __be32` — big-endian annotated u32 |
| `__be64` | `linux/types.h` | `typedef __u64 __be64` — big-endian annotated u64 |

Add as a new `linux.types` partition in `bnd-linux-gen/linux.toml`. Then reference via `--reference bnd_linux,full,linux`.

---

## 5. Partition Configuration (Actual)

```toml
# bnd-rdma-gen/rdma.toml

include_paths = ["/usr/include/x86_64-linux-gnu", "/usr/include"]

[output]
name = "rdma"
file = "bnd-rdma.winmd"

# Import POSIX types from pre-built winmd
[[type_import]]
winmd = "../../bnd/bnd-posix/winmd/bnd-posix.winmd"
namespace = "posix"

# Import Linux types (__be16/32/64) from pre-built winmd
[[type_import]]
winmd = "../../bnd/bnd-linux/winmd/bnd-linux.winmd"
namespace = "linux"

# Partition 1: ibverbs — core RDMA verb API
[[partition]]
namespace = "rdma.ibverbs"
library = "ibverbs"
headers = ["infiniband/verbs.h"]
traverse = [
    "infiniband/verbs.h",
    "infiniband/verbs_api.h",
    "infiniband/ib_user_ioctl_verbs.h",
    "rdma/ib_user_verbs.h",
]

# Partition 2: rdmacm — RDMA Connection Manager API
[[partition]]
namespace = "rdma.rdmacm"
library = "rdmacm"
headers = ["rdma/rdma_cma.h"]
traverse = [
    "rdma/rdma_cma.h",
    "infiniband/sa.h",
]

# Partition 3: wrapper — C wrappers for static inline functions
# wrapper.h includes verbs.h but we only traverse wrapper.h,
# so only the rdma_wrap_* symbols are extracted.
[[partition]]
namespace = "rdma.wrapper"
library = "rdma_wrapper"
headers = ["../rdma-io-sys/wrapper/wrapper.h"]
traverse = ["../rdma-io-sys/wrapper/wrapper.h"]
```

Key design points:
- **Traverse paths control extraction scope**: ibverbs traverses 4 headers to capture all kernel UAPI types; rdmacm traverses `sa.h` for path record types; wrapper only traverses `wrapper.h` to avoid re-extracting verbs types.
- **`type_import` paths are relative to the TOML file**: `../../bnd/` goes from `bnd-rdma-gen/` up to the workspace root, then into the sibling `bnd` repo.
- **Wrapper header path uses `../rdma-io-sys/`**: bnd resolves `headers` relative to the TOML file's directory (`bnd-rdma-gen/`).

---

## 6. C Wrapper for Inline Functions

The 96 inline functions from `verbs.h` are wrapped in `rdma-io-sys/wrapper/wrapper.c` and compiled by `cc` at build time. The wrapper header is also a bnd partition, so Rust FFI declarations are auto-generated alongside the main bindings.

### wrapper.h (actual)

```c
#pragma once

#ifdef __cplusplus
extern "C" {
#endif

#include <infiniband/verbs.h>

// All wrappers use rdma_wrap_ prefix
void rdma_wrap_ibv_wr_send(struct ibv_qp_ex *qp);
int rdma_wrap_ibv_poll_cq(struct ibv_cq *cq, int num_entries, struct ibv_wc *wc);
int rdma_wrap_ibv_post_send(struct ibv_qp *qp, struct ibv_send_wr *wr,
                            struct ibv_send_wr **bad_wr);
// ... 96 wrappers total

#ifdef __cplusplus
}
#endif
```

### wrapper.c (actual)

```c
#include <infiniband/verbs.h>
#include "wrapper.h"

void rdma_wrap_ibv_wr_send(struct ibv_qp_ex *qp) {
    ibv_wr_send(qp);
}

int rdma_wrap_ibv_poll_cq(struct ibv_cq *cq, int num_entries, struct ibv_wc *wc) {
    return ibv_poll_cq(cq, num_entries, wc);
}

int rdma_wrap_ibv_post_send(struct ibv_qp *qp, struct ibv_send_wr *wr,
                            struct ibv_send_wr **bad_wr) {
    return ibv_post_send(qp, wr, bad_wr);
}
// ... all 96 inline functions wrapped similarly
```

### Key design decisions

- **`#include <infiniband/verbs.h>` in wrapper.h**: Rather than forward declarations, we include the full header. bnd handles this correctly because the `traverse` list only traverses `wrapper.h` — types from `verbs.h` are resolved via the ibverbs partition, not re-extracted.
- **`rdma_wrap_` prefix**: Avoids symbol collision with any future libibverbs exports.
- **Wrapper as bnd partition**: The wrapper declarations are extracted as `rdma.wrapper` partition, generating `bnd_rdma::rdma::wrapper::*` Rust FFI functions with proper cross-module type references (e.g., `super::ibverbs::ibv_qp`).

---

## 7. Crate Structure (Actual)

All crates live in the `rust-rdma-io` workspace. The `bnd` repo at `../bnd` provides shared tooling and runtime crates as path dependencies. Generated FFI modules live directly in `rdma-io-sys` (no separate generated crate).

```
rust-rdma-io/                       # Workspace root
├── Cargo.toml                      # Workspace with path deps to ../bnd/
├── rust-toolchain.toml             # Rust 1.93.0
│
├── bnd-rdma-gen/                   # Generator (dev-time only)
│   ├── Cargo.toml                  # depends on bnd-winmd, windows-bindgen
│   ├── rdma.toml                   # 3-partition bnd config
│   └── src/
│       ├── lib.rs                  # generate(output_dir) function
│       └── main.rs                 # CLI: cargo run -p bnd-rdma-gen
│
├── rdma-io-sys/                    # Sys crate (user-facing, includes generated FFI)
│   ├── Cargo.toml                  # depends on bnd-posix, bnd-linux, windows-link, cc
│   ├── build.rs                    # Compiles wrapper.c, links ibverbs + rdmacm
│   ├── winmd/
│   │   └── bnd-rdma.winmd          # Generated .winmd (checked in)
│   ├── src/
│   │   ├── lib.rs                  # pub mod rdma; pub use rdma::*;
│   │   └── rdma/
│   │       ├── mod.rs              # Feature-gated: ibverbs, rdmacm, wrapper
│   │       ├── ibverbs/mod.rs      # 4163 lines: 275 structs, 92 enums, 164 fns
│   │       ├── rdmacm/mod.rs       # 465 lines: 19 structs, 4 enums, 40 fns
│   │       └── wrapper/mod.rs      # 200 lines: 96 wrapper fn declarations
│   └── wrapper/
│       ├── wrapper.h               # 108 lines: rdma_wrap_* declarations
│       └── wrapper.c               # 386 lines: delegates to inline functions
│
└── rdma-io-tests/                  # Integration tests (siw)
    ├── Cargo.toml                  # depends on rdma-io-sys
    └── src/
        └── lib.rs                  # 8 siw control-path tests
```

### Cargo.toml (workspace)

```toml
[workspace]
resolver = "2"
members = ["bnd-rdma-gen", "rdma-io-sys", "rdma-io-tests"]

[workspace.dependencies]
bnd-winmd = { path = "../bnd/bnd-winmd" }
bnd-posix = { path = "../bnd/bnd-posix" }
bnd-linux = { path = "../bnd/bnd-linux" }
windows-bindgen = "0.66"
windows-link = "0.2"
```

### Generation logic (bnd-rdma-gen/src/lib.rs)

Following the bnd-openssl pattern — the key is `--reference` flags:

```rust
pub fn generate(output_dir: &Path) {
    let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Step 1: Extract C headers → .winmd
    let rdma_winmd = output_dir.join("winmd/bnd-rdma.winmd");
    bnd_winmd::run(&gen_dir.join("rdma.toml"), Some(&rdma_winmd)).unwrap();

    // Step 2: Locate cross-reference winmds
    let posix_winmd = gen_dir.join("../../bnd/bnd-posix/winmd/bnd-posix.winmd");
    let linux_winmd = gen_dir.join("../../bnd/bnd-linux/winmd/bnd-linux.winmd");

    // Step 3: Generate Rust bindings with cross-crate references
    windows_bindgen::bindgen([
        "--in", rdma_winmd.to_str().unwrap(),
        "--in", posix_winmd.to_str().unwrap(),
        "--in", linux_winmd.to_str().unwrap(),
        "--out", output_dir.to_str().unwrap(),
        "--filter", "rdma",
        "--reference", "bnd_posix,full,posix",   // posix types → bnd_posix::posix::*
        "--reference", "bnd_linux,full,linux",    // __be types → bnd_linux::linux::*
        "--sys", "--package", "--no-toml",
    ]).unwrap();
}
```

### Generation workflow (developer-time, not build-time)

```bash
# Regenerate when rdma-core headers change:
cargo run -p bnd-rdma-gen    # → rdma-io-sys/winmd/ + rdma-io-sys/src/rdma/
git add rdma-io-sys/         # Check in generated code
```

### Build workflow (user-time)

```bash
cargo build
# build.rs (rdma-io-sys):
#   1. cc compiles wrapper/wrapper.c → librdma_wrapper.a
#   2. Links librdma_wrapper.a (static) + libibverbs.so + librdmacm.so (dynamic)
#   3. Pre-generated Rust bindings in rdma-io-sys/src/rdma/ — no codegen at build time
```

---

## 8. Comparison with Pure bindgen Approach

| Aspect | bindgen (Option D) | bnd + wrapper |
|---|---|---|
| Build-time codegen | bindgen runs every build | None — pre-generated |
| Build deps | `libibverbs-dev` + `libclang` | `libibverbs-dev` only (for cc) |
| Inline functions | wrapper.c (same) | wrapper.c (same) |
| Output quality | Flat `bindings.rs`, raw types | Namespaced modules, cleaner types |
| Type reuse | Standalone | Shares types with `bnd-posix` ecosystem |
| Maintenance | Re-generate on header change | Re-generate on header change (same) |
| docs.rs | Needs pre-generated fallback | Works out of the box (checked in) |
| Compile time | Slower (bindgen + cc) | Faster (cc only) |

---

## 9. Wrapper Naming Convention

All wrapper functions use the `rdma_wrap_` prefix to avoid symbol collisions:

```
ibv_post_send             → rdma_wrap_ibv_post_send
ibv_poll_cq               → rdma_wrap_ibv_poll_cq
ibv_wr_start              → rdma_wrap_ibv_wr_start
ibv_start_poll            → rdma_wrap_ibv_start_poll
ibv_wc_read_opcode        → rdma_wrap_ibv_wc_read_opcode
___ibv_query_port         → rdma_wrap____ibv_query_port
__ibv_reg_mr              → rdma_wrap___ibv_reg_mr
```

The safe Rust wrapper layer (future `rdma-io` crate) will map these back to ergonomic names.

---

## 10. bnd Modifications Made

### bnd-linux (`../bnd/bnd-linux-gen/linux.toml`)

✅ **Added `linux.types` partition** — extracts `__be16`, `__be32`, `__be64`, `__le16`, `__le32`, `__le64`, `__poll_t`, `__sum16`, `__wsum` from `linux/types.h`. Committed to bnd main branch.

### bnd-winmd

No modifications needed. Anonymous unions and bitfields were handled correctly by the existing extraction logic.

---

## 11. Resolved Questions

1. **Wrapper granularity**: All 96 inline functions wrapped upfront. The cost is minimal (386 lines of C) and avoids incremental maintenance.
2. **bnd partition split**: Three partitions — ibverbs, rdmacm, wrapper. Sub-partitioning within ibverbs was not needed.
3. **Version pinning**: Initial generation against rdma-core 50.0 (Ubuntu 24.04).
4. **Wrapper header approach**: `#include <infiniband/verbs.h>` directly. bnd's `traverse` mechanism correctly limits extraction scope.
5. **Crate location**: All crates in `rust-rdma-io` workspace (not in `../bnd`). The gen crate references `../bnd` winmds via relative paths.
6. **Single sys crate**: Generated FFI modules merged into `rdma-io-sys` directly (no separate `bnd-rdma` crate). Keeps `build.rs`, `wrapper.c`, and `link!()` declarations in the same crate. `windows-link` is a no-op on Linux.
