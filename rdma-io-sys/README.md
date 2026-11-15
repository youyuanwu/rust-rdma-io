# rdma-io-sys

Part of [rdma-io](../README.md) - safe Rust bindings for RDMA programming.

Low-level FFI bindings for [libibverbs](https://github.com/linux-rdma/rdma-core)
and [librdmacm](https://github.com/linux-rdma/rdma-core), generated at build time
with [bindgen](https://rust-lang.github.io/rust-bindgen/).

## Prerequisites

```sh
sudo apt install build-essential libclang-dev libibverbs-dev librdmacm-dev
```

libclang is required for every build. Set `LIBCLANG_PATH` if it is installed
outside the default search paths.

## Binding generation

The build script parses the installed verbs and RDMA CM headers together with
`wrapper/wrapper.h`, writes bindings into Cargo's `OUT_DIR`, and compiles
`wrapper/wrapper.c` using `cc`. Included headers and wrapper sources are tracked
for rebuilds. No generated Rust or metadata files need to be checked in.

Many libibverbs operations are `static inline` functions without linkable
symbols. The C wrapper exports them as `rdma_wrap_*` functions. The crate
links this wrapper statically and `libibverbs`/`librdmacm` dynamically.

All bindings, including wrappers, are available at the crate root without
feature selection. Socket, pthread, and timespec types use `libc`. See the
[binding design](../docs/design/Bindings.md) for cross-compilation and
migration notes.

## Usage

Prefer [`rdma-io`](../rdma-io/) for a safe, high-level API.

```rust
use rdma_io_sys::{ibv_get_device_list, rdma_create_id, rdma_wrap_ibv_post_send};
```
