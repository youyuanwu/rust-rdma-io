# RDMA Bindings via bindgen

`rdma-io-sys` generates Rust FFI bindings during its Cargo build using
[bindgen](https://rust-lang.github.io/rust-bindgen/). It parses the installed
`infiniband/verbs.h`, `rdma/rdma_cma.h`, and the local
`rdma-io-sys/wrapper/wrapper.h` in one translation unit, so all declarations
share the same RDMA types.

## Build pipeline

1. bindgen reads the headers for the Cargo target and writes `bindings.rs`
   into Cargo's `OUT_DIR`.
2. `cc` compiles `wrapper/wrapper.c` into `librdma_wrapper.a`.
3. Rust includes the generated bindings and links the wrapper, `libibverbs`,
   and `librdmacm`.

The generated bindings are not checked in. Cargo tracks included headers
through `bindgen::CargoCallbacks`; changes to headers or wrapper sources
trigger regeneration. There is no separate generator crate, metadata
download, or manual regeneration command.

## Prerequisites

```sh
sudo apt install build-essential libclang-dev libibverbs-dev librdmacm-dev
cargo build -p rdma-io-sys
```

libclang is required for every build, not only for development. Set
`LIBCLANG_PATH` when it is installed outside the default search paths.
Cross-compilation also requires target RDMA headers and libraries, a target
C compiler for `cc`, and Clang's target sysroot/include configuration, supplied
through bindgen's `BINDGEN_EXTRA_CLANG_ARGS` (or its target-specific variant).

## Rust API and Linux types

All bindings are exported directly at the `rdma_io_sys` crate root. C enum
values retain their original names, such as `IBV_QPT_RC`, and bindgen derives
`Default` for zero-initialized attributes and work requests.

System socket, pthread, and timespec types use `libc`, rather than private
copies generated from system headers. This keeps CM address pointers
compatible with `libc::sockaddr` and preserves target-specific layouts.
Bindgen's generated layout assertions remain enabled.

Anonymous unions use bindgen's `__bindgen_anon_*` fields. The safe `rdma-io`
layer accesses these fields internally and retains its high-level API.

## Inline functions

libibverbs exposes many operations as `static inline` functions, which have
no exported symbol for Rust to link. The existing C shim exports these as
`rdma_wrap_*` functions. Its header is included in the bindgen translation
unit, keeping wrapper signatures consistent with the verbs declarations.

## Migration from bnd

The bnd generator, checked-in Rust bindings, WinMD metadata, and generated
per-header Cargo features have been removed. Replace imports from
`rdma_io_sys::{ibverbs, rdmacm, wrapper}` or `rdma_io_sys::rdma::*` with
imports from `rdma_io_sys`. No sys-crate feature selection is needed.
