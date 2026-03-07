# RDMA Bindings Strategy — Research & Analysis

> Surveyed Feb 2026. Based on analysis of 5 Rust RDMA sys crates.

---

## 1. The Core Challenge: libibverbs Inline Functions

The fundamental problem every Rust RDMA binding must solve:

**Most critical ibverbs data-path functions are `static inline` in `verbs.h`**. They don't exist as real symbols in `libibverbs.so` — they're expanded at C compile time and dispatch through function pointers stored in opaque structs.

```c
// From rdma-core/libibverbs/verbs.h (simplified)
static inline int ibv_post_send(struct ibv_qp *qp,
                                struct ibv_send_wr *wr,
                                struct ibv_send_wr **bad_wr) {
    return qp->context->ops.post_send(qp, wr, bad_wr);  // function pointer!
}

static inline int ibv_poll_cq(struct ibv_cq *cq, int n, struct ibv_wc *wc) {
    return cq->context->ops.poll_cq(cq, n, wc);          // function pointer!
}
```

**Affected hot-path functions** (all inline in verbs.h):
- `ibv_post_send`, `ibv_post_recv` — post work requests
- `ibv_poll_cq` — poll completions
- `ibv_req_notify_cq` — request CQ notification
- `ibv_post_srq_recv` — post to shared receive queue
- `ibv_wr_start`, `ibv_wr_send`, `ibv_wr_complete` — new WR API
- `ibv_start_poll`, `ibv_next_poll`, `ibv_end_poll` — new CQ API

**Why bindgen can't handle them**: `bindgen` skips `static inline` functions by default. Even with `generate_inline_functions(true)`, the generated code may not work because the functions dereference provider-specific struct layouts set up at runtime.

---

## 2. How Each Project Solves This

### Summary Matrix

| Project | Compile-time rdma-core? | Inline functions | Union handling | Linking | Headers |
|---|---|---|---|---|---|
| **ibverbs-sys** | Vendored (cmake configure only) | Rust fn-ptr wrappers in safe layer | Manual `ibv_wc` | Dynamic `libibverbs.so` | Vendored submodule |
| **rdma-sys** | System required (`pkg-config`) | Manual Rust `#[inline]` wrappers | Manual (unions, send_wr, wc) | Dynamic | System headers |
| **rdma-mummy-sys** | ❌ Not required | C stub wrappers via dlopen/dlsym | Via bindgen + stubs | Static stub → dlopen | Bundled "mummy" headers |
| **Nugine/rdma** | System required (`pkg-config`) | Manual Rust `#[inline]` (~60 fns) | Bindgen + blocklist | Dynamic | System headers |
| **rrddmma** | Fallback chain (MLNX→pkg→build) | Manual Rust `#[inline]` | Feature-gated modules | Dynamic or static | Version-dependent |

---

### 2.1 ibverbs-sys (rust-ibverbs)

**Strategy**: Vendor rdma-core as git submodule, cmake-configure to generate headers, bindgen against vendored headers.

```
Build flow:
  vendor/rdma-core (git submodule)
    → cmake configure (headers only, no library build)
    → bindgen reads vendored verbs.h
    → OUT_DIR/bindings.rs
  
  Runtime: links dynamically against system libibverbs.so
```

**Inline function solution**: Doesn't wrap them at the sys level. The safe `ibverbs` crate calls function pointers directly:
```rust
// In ibverbs/src/lib.rs (safe wrapper)
let ctx = unsafe { *self.qp }.context;
let ops = &mut unsafe { *ctx }.ops;
let errno = unsafe {
    ops.post_send.as_mut().unwrap()(self.qp, &mut wr, &mut bad_wr)
};
```

**Pros**: Self-contained (vendored headers), no system deps at compile time for header generation.
**Cons**: Git submodule management, cmake required, inline functions leak to safe layer.

---

### 2.2 rdma-sys (datenlord/async-rdma)

**Strategy**: Require system `libibverbs-dev` + `librdmacm-dev`, bindgen against system headers, manual wrappers for inlines.

```
Build flow:
  pkg-config finds libibverbs >= 1.8.28, librdmacm >= 1.2.28
    → bindgen reads system infiniband/verbs.h + rdma/rdma_cma.h
    → blocklist problematic types (ibv_wc, ibv_send_wr, ibv_async_event)
    → OUT_DIR/bindings.rs
  
  Manual code in verbs.rs:
    ibv_post_send, ibv_poll_cq, etc. as #[inline] Rust fns
  
  Manual code in types.rs:
    ibv_wc, ibv_send_wr with unions as hand-written #[repr(C)]
```

**Inline function solution**: ~15 manual Rust wrappers calling function pointers:
```rust
#[inline]
pub unsafe fn ibv_poll_cq(cq: *mut ibv_cq, num_entries: c_int, wc: *mut ibv_wc) -> c_int {
    (*cq).poll_cq.unwrap()(cq, num_entries, wc)
}
```

**Pros**: Simple, minimal dependencies.
**Cons**: Requires system `libibverbs-dev` at compile time, union types manually maintained.

---

### 2.3 rdma-mummy-sys (sideway) — Most Innovative

**Strategy**: Bundle a "mummy" C stub library that wraps all ibverbs/rdmacm functions through dlopen/dlsym indirection.

```
Build flow:
  rdma-core-mummy/ (bundled submodule)
    → cmake builds stub C library (libibverbs.a, librdmacm.a)
    → Stubs use dlopen("libibverbs.so.1") at runtime
    → Each ibv_* function is a real symbol calling through fn pointer
    → bindgen generates normal extern "C" bindings
    → No inline function problem!
  
  Runtime:
    Stub constructor: dlopen("libibverbs.so.1", RTLD_LAZY)
    dlsym loads real versioned symbols
    If .so missing: functions return EOPNOTSUPP (graceful degradation)
```

**The mummy technique** (key innovation):
```c
// rdma-core-mummy/src/ibverbs.c
static __attribute__((constructor)) void ibverbs_init(void) {
    void *handle = dlopen("libibverbs.so.1", RTLD_LAZY | RTLD_LOCAL);
    LOAD_FUNC_PTR_IBV(handle, get_device_list);
    LOAD_FUNC_PTR_IBV(handle, post_send);     // Even inline functions!
    LOAD_FUNC_PTR_IBV(handle, poll_cq);
    // ...50+ functions
}

// Generates real callable symbols:
DEFINE_WRAPPER_FUNC_IBV(post_send, int,
    struct ibv_qp *qp, struct ibv_send_wr *wr, struct ibv_send_wr **bad_wr) {
    RETURN_NOT_EXIST(post_send, -1);
    return FUNC_PTR(post_send)(qp, wr, bad_wr);
}
```

**Wait — how does dlsym find `ibv_post_send` if it's inline?**

It doesn't find the inline version. Modern rdma-core actually *also* exports these functions as proper symbols with version tags (e.g., `ibv_post_send@IBVERBS_1.0`) for backwards compatibility. The mummy stubs dlsym these versioned symbols.

**Pros**: No compile-time rdma-core needed, works with any OFED version at runtime, graceful degradation, no inline function problem, new ibverbs API support.
**Cons**: Extra dlopen/dlsym indirection (one-time cost at init), maintains C stub code, bundled submodule.

---

### 2.4 Nugine/rdma

**Strategy**: Similar to rdma-sys but with higher minimum version requirement and more comprehensive inline wrappers.

```
Build flow:
  pkg-config finds libibverbs >= 1.14.41, librdmacm >= 1.3.41
    → bindgen reads system headers
    → blocklist: ibv_reg_mr, ibv_query_port (manual versions)
    → OUT_DIR/generated.rs
  
  Manual code in bindings/ibverbs.rs:
    ~60 inline function wrappers
    verbs_get_ctx_op! macro for version-safe dispatch
    container_of_mut! macro for pointer arithmetic
  
  Pre-generated bindings for docs.rs:
    src/bindings/x86_64_unknown_linux_gnu.rs
```

**Inline function solution**: Most comprehensive manual Rust wrapper set (~60 functions), including extended CQ API, version-aware routing:
```rust
#[inline]
pub unsafe fn ibv_create_cq_ex(
    context: *mut ibv_context,
    cq_attr: *mut ibv_cq_init_attr_ex,
) -> *mut ibv_cq_ex {
    let vctx = verbs_get_ctx_op!(context, create_cq_ex);
    if vctx.is_null() {
        set_errno(EOPNOTSUPP);
        return ptr::null_mut();
    }
    let op = (*vctx).create_cq_ex.unwrap_unchecked();
    (op)(context, cq_attr)
}
```

**Pros**: Higher minimum version enables modern features, comprehensive coverage, pre-generated bindings for docs.rs.
**Cons**: Requires system rdma-core at compile time, large manual wrapper surface.

---

### 2.5 rrddmma

**Strategy**: Multi-target with fallback chain: MLNX_OFED v4.x → system rdma-core → build from source.

```
Detection chain:
  1. legacy feature + ofed_info → MLNX_OFED v4.x (experimental APIs)
  2. pkg-config libibverbs >= 1.8.28 → standard rdma-core
  3. fallback: clone + cmake rdma-core, static link mlx5 only

Feature-gated binding modules:
  bindings/legacy.rs  — MLNX_OFED v4.x ABI (different ibv_wc layout!)
  bindings/exp.rs     — DC QPs, extended atomics
  bindings/rdma_core.rs — standard rdma-core ABI
  bindings/common.rs  — shared inline wrappers
```

**Pros**: Supports vendor-specific extensions (DC, ext atomics), flexible.
**Cons**: Complex build, ABI differences between modules, academic focus.

---

## 3. The Two ibverbs API Generations

### Legacy API (all existing Rust libs except sideway)

```c
// Struct-based: fill out linked list, submit batch
struct ibv_send_wr wr = { .opcode = IBV_WR_SEND, ... };
ibv_post_send(qp, &wr, &bad_wr);  // inline → fn pointer

struct ibv_wc wc;
ibv_poll_cq(cq, 1, &wc);          // inline → fn pointer
```

- Functions are inline, calling function pointers in `ibv_context.ops`
- Struct-based work request construction
- Every Rust binding must manually wrap these

### New API (only sideway wraps this)

```c
// Function-based: builder pattern, batch critical section
ibv_wr_start(qp);                  // inline → fn pointer in qp_ex
ibv_wr_send(qp);                   // inline → fn pointer
ibv_wr_set_sge(qp, lkey, addr, len);
ibv_wr_complete(qp);               // inline → fn pointer

// New CQ polling
ibv_start_poll(cq_ex, &attr);      // inline → fn pointer in cq_ex
ibv_wc_read_opcode(cq_ex);         // inline
ibv_next_poll(cq_ex);              // inline
ibv_end_poll(cq_ex);               // inline
```

- Also inline functions calling through `ibv_qp_ex` / `ibv_cq_ex` vtables
- Lower overhead (no struct allocation), better batching
- More extensible for future hardware features
- **Requires rdma-core >= ~25+ (libibverbs >= 1.5+)**

---

## 4. Strategy Options for Our Library

### Option A: System Headers + Manual Wrappers (rdma-sys / Nugine approach)

```
Compile: pkg-config → system headers → bindgen → manual inline wrappers
Runtime: dynamic link libibverbs.so
```

| Aspect | Assessment |
|---|---|
| Complexity | Low-medium |
| Compile deps | libibverbs-dev, librdmacm-dev required |
| CI setup | Need apt-get install before build |
| Cross-compile | Difficult (need target headers) |
| Inline functions | ~60 manual Rust wrappers to maintain |
| New API support | Must manually wrap ibv_wr_* functions too |
| docs.rs | Need pre-generated bindings fallback |

### Option B: Vendored Headers + Bindgen (rust-ibverbs approach)

```
Compile: git submodule rdma-core → cmake configure → bindgen → manual inlines
Runtime: dynamic link libibverbs.so
```

| Aspect | Assessment |
|---|---|
| Complexity | Medium |
| Compile deps | cmake, clang (for cmake configure) |
| CI setup | Simpler (headers vendored) |
| Cross-compile | Better (headers available, cmake can target) |
| Inline functions | Same manual wrapper problem |
| Version control | Pinned to submodule version |
| Repo size | Larger (rdma-core submodule ~50MB) |

### Option C: Mummy/Stub Approach (sideway approach)

```
Compile: bundled C stubs + dlopen → bindgen against stub headers → no inline problem
Runtime: dlopen("libibverbs.so.1") → dlsym real symbols
```

| Aspect | Assessment |
|---|---|
| Complexity | Medium-high (maintain C stubs) |
| Compile deps | cmake, clang (for stub build) — NO rdma-core! |
| CI setup | Easiest (no system rdma packages for compile) |
| Cross-compile | Best (fully self-contained headers) |
| Inline functions | **Solved** — stubs expose real symbols |
| New API support | Stubs can wrap ibv_wr_* functions |
| Graceful degradation | If no rdma-core at runtime → EOPNOTSUPP |
| docs.rs | Works out of the box (stubs compile anywhere) |

### Option D: C Wrapper + System Headers ⭐ Chosen

```
Compile: write small wrapper.c that calls inline functions → cc crate → bindgen against system headers
Runtime: wrapper.a linked statically, libibverbs.so dynamically
```

```c
// wrapper.c
#include <infiniband/verbs.h>

int rdma_io_post_send(struct ibv_qp *qp, struct ibv_send_wr *wr,
                      struct ibv_send_wr **bad_wr) {
    return ibv_post_send(qp, wr, bad_wr);
}

int rdma_io_poll_cq(struct ibv_cq *cq, int n, struct ibv_wc *wc) {
    return ibv_poll_cq(cq, n, wc);
}
// ... all inline functions get real symbols
```

| Aspect | Assessment |
|---|---|
| Complexity | **Low** — simplest correct solution |
| Compile deps | `libibverbs-dev`, `librdmacm-dev` (system packages) |
| CI setup | `apt-get install libibverbs-dev librdmacm-dev` |
| Inline functions | **Solved** — C compiler expands them into real symbols |
| Overhead | One extra function call (negligible) |
| Maintenance | Small C file (~50 lines), easy to audit |
| docs.rs | Pre-generated bindings fallback (same as Nugine/rdma) |

### Option E: C Wrapper + Vendored Headers (Rejected)

Same as Option D but vendors a copy of `verbs.h` instead of using system headers.

**Rejected**: Adds header maintenance burden without meaningful benefit. Anyone building an RDMA library already needs `libibverbs.so` at runtime, so requiring the `-dev` package at build time is not an additional burden. Cross-compilation and docs.rs can be handled with pre-generated bindings.

---

## 5. Recommendation

**Option D (C wrapper + system headers)** — implemented with bnd for Rust FFI generation.

### What was built

Two crates in the `rust-rdma-io` workspace:

1. **`bnd-rdma-gen`** — Developer-time generator. Extracts RDMA types from system headers via `bnd-winmd` into a `.winmd`, then runs `windows-bindgen` to emit Rust FFI modules. Three partitions: ibverbs (275 structs, 92 enums, 164 functions), rdmacm (19 structs, 4 enums, 40 functions), wrapper (96 inline function wrappers).

2. **`rdma-io-sys`** — User-facing sys crate. Contains the generated FFI modules (`src/rdma/`) directly — no separate generated crate. Compiles `wrapper.c` (96 C wrappers for static inline functions) via `cc` crate. Cross-references `bnd-posix` (pthread, socket, inet types) and `bnd-linux` (`__be16/32/64`) via `--reference` flags.

### Build flow:
```
cargo build (rdma-io-sys):
  1. cc compiles wrapper/wrapper.c → librdma_wrapper.a (static)
  2. Links libibverbs.so + librdmacm.so (dynamic)
  3. Pre-generated Rust bindings in rdma-io-sys/src/rdma/ — no codegen at build time
```

See [BndBindings.md](BndBindings.md) for full implementation details.

---

## 6. Header / API Coverage Plan

Regardless of binding approach, we need to decide what to bind:

### Must have (day 1)
| Header | Functions | Notes |
|---|---|---|
| `infiniband/verbs.h` | Core ibverbs: device, context, PD, CQ, QP, MR, SRQ, AH | The whole ibverbs API |
| `rdma/rdma_cma.h` | Connection manager: resolve, connect, listen, accept | For TCP-like connection setup |

### Should have
| Header | Functions | Notes |
|---|---|---|
| `infiniband/verbs.h` | New WR API: `ibv_wr_start`, `ibv_wr_send`, etc. | Modern posting API |
| `infiniband/verbs.h` | Extended CQ: `ibv_start_poll`, `ibv_next_poll` | Modern polling API |
| `rdma/rdma_verbs.h` | Verbs-level CM helpers | Simplified CM usage |

### Nice to have
| Header | Functions | Notes |
|---|---|---|
| `infiniband/verbs.h` | Device memory (DM), memory windows (MW) | Advanced features |
| Provider-specific | `mlx5dv_*` | Mellanox direct verbs (future) |

---

## 7. Open Questions

1. **Minimum rdma-core version**: What's the oldest rdma-core we want to support? (affects which APIs are available)
   - Ubuntu 20.04 ships rdma-core 28.0 (libibverbs 1.8.28)
   - Ubuntu 22.04 ships rdma-core 39.0
   - RHEL 8 ships rdma-core 32.0
2. **Static vs dynamic linking**: Should we support static linking for single-binary deployments?
3. **Provider-specific APIs**: Should we plan for `mlx5dv_*` (NVIDIA direct verbs) from the start?
4. **ABI stability**: The `verbs_context` internal layout changes between rdma-core versions — how to handle?

## 8. Resolved Decisions

1. **Binding strategy**: Option D — C wrapper + system headers + bnd (not bindgen). (Resolved & implemented: Feb 2026)
2. **No vendored headers**: System `libibverbs-dev` / `librdmacm-dev` required at build time. (Resolved: Feb 2026)
3. **No mummy/dlopen approach**: Direct linking against system `libibverbs.so` is simpler and sufficient. (Resolved: Feb 2026)
4. **Code generation tool**: bnd (not bindgen) — pre-generated, namespaced modules, type sharing with `bnd-posix`/`bnd-linux`. (Resolved & implemented: Feb 2026)
5. **Wrapper naming**: `rdma_wrap_` prefix (e.g., `rdma_wrap_ibv_post_send`). (Resolved & implemented: Feb 2026)
6. **Wrapper as bnd partition**: `wrapper.h` is a third bnd partition, so Rust FFI declarations are auto-generated with cross-module references. (Resolved & implemented: Feb 2026)
7. **`__be*` types**: Added `linux.types` partition to bnd-linux upstream. (Resolved & committed: Feb 2026)
8. **Single sys crate**: Merged `bnd-rdma` into `rdma-io-sys` — generated FFI modules live alongside `build.rs` and `wrapper.c`. No separate generated crate. (Resolved: Feb 2026)
