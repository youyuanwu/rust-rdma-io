# RDMA Bindings via bnd

`rdma-io-sys` uses the bnd 0.0.8 direct-Clang toolchain to generate checked-in
Rust FFI bindings for rdma-core.

## Pipeline

```text
verbs.h + rdma_cma.h + wrapper.h
             |
             v
         bnd-clang
             |
             v
     per-header RDL files
             |
             v
        windows-rdl
             |
             v
       flat temporary WinMD
             |
             v
 bnd-clang remap_by_header()
             |
             v
   canonical bnd-rdma.winmd
             |
             v
         bnd-bindgen
             |
             v
 rdma-io-sys/src/rdma/*
```

The generator parses one coherent C translation unit and emits RDL grouped by
the header that defines each item. `windows_clang::remap_by_header()` makes
that ownership canonical and repairs external metadata scopes through an RDL
round trip. The checked-in WinMD namespaces therefore match the generated Rust
modules and Cargo features.

The primary generated modules are:

- `rdma::verbs`, `rdma::verbs_api`
- `rdma::ib_user_verbs`, `rdma::ib_user_ioctl_verbs`
- `rdma::rdma_cma`, `rdma::sa`
- `rdma::wrapper`

`rdma-io-sys` retains `ibverbs` and `rdmacm` facade modules that re-export the
corresponding header-owned modules for existing consumers.

## Toolchain

The generator uses the published bnd 0.0.8 crates:

```toml
bnd-clang = "0.0.8"
bnd-bindgen = "0.0.8"
bnd-linux = "0.0.8"
bnd-macros = "0.0.6"
windows-metadata = "0.100"
windows-rdl = { version = "0.100", default-features = false }
```

`bnd-clang` and `bnd-bindgen` retain the Rust library names
`windows_clang` and `windows_bindgen`.

## Canonical header ownership

`bnd-rdma-gen/src/clang.rs` defines root headers, partition headers, and native
library policy. Representative ownership is:

| Header | Rust module | Native library |
|---|---|---|
| `infiniband/verbs.h` | `rdma::verbs` | `ibverbs` |
| `infiniband/verbs_api.h` | `rdma::verbs_api` | `ibverbs` |
| `rdma/ib_user_verbs.h` | `rdma::ib_user_verbs` | `ibverbs` |
| `rdma/rdma_cma.h` | `rdma::rdma_cma` | `rdmacm` |
| `infiniband/sa.h` | `rdma::sa` | `ibverbs` |
| `rdma-io-sys/wrapper/wrapper.h` | `rdma::wrapper` | `rdma_wrapper` |

All headers are parsed together so shared RDMA declarations have one owner and
cross-module references remain consistent.

## Shared Linux types

The canonical bnd-linux 0.0.8 metadata uses the same defining-header
namespaces as the Rust crate:

| C type | Rust path |
|---|---|
| `__be16`, `__be32`, `__be64`, `ssize_t` | `bnd_linux::libc::types::*` |
| `pthread_mutex_t`, `pthread_cond_t` | `bnd_linux::libc::pthreadtypes::*` |
| `sockaddr`, `sockaddr_storage` | `bnd_linux::libc::socket::*` |
| `sockaddr_in`, `sockaddr_in6` | `bnd_linux::libc::in_::*` |
| `socklen_t` | `bnd_linux::libc::unistd::socklen_t` |
| `timespec` | `bnd_linux::libc::struct_timespec::timespec` |

The Linux WinMD is supplied to Clang, RDL compilation, canonical remapping,
and bindgen. Remapping imports its header namespaces to preserve external
TypeRefs. Bindgen then uses one namespace-level full reference:

```rust
bindgen.reference(
    "bnd_linux",
    windows_bindgen::ReferenceStyle::Full,
    "libc",
);
```

This replaces the per-type ownership table required by bnd 0.0.7.

## Inline function wrappers

libibverbs exposes many operations as `static inline` functions, which cannot
be linked directly from Rust. `rdma-io-sys/wrapper/wrapper.c` provides exported
`rdma_wrap_*` functions, and `wrapper.h` is included in the same generation
translation unit. This keeps wrapper signatures tied to the generated verbs
types.

The `rdma-io-sys` build script compiles the C wrapper into
`librdma_wrapper.a`. Normal consumers compile only the checked-in Rust bindings
and C wrapper; they do not run Clang or the Rust binding generator.

## Regeneration

Install the generator prerequisites:

```sh
sudo apt install libclang-dev libibverbs-dev librdmacm-dev
```

Then run:

```sh
just gen-bindings
```

The recipe downloads the canonical bnd-linux 0.0.8 WinMD from the release
commit, verifies its SHA-256 digest, and runs `bnd-rdma-gen` with one Cargo
build job.

Generation updates:

- `rdma-io-sys/winmd/bnd-rdma.winmd`
- `rdma-io-sys/src/rdma/mod.rs`
- header-owned modules below `rdma-io-sys/src/rdma/`
- generated feature dependencies below the marker in
  `rdma-io-sys/Cargo.toml`

Do not edit generated files directly.

## ABI and source compatibility

The direct-Clang pipeline derives native integer widths from Clang. C
`size_t` parameters therefore generate as Rust `usize`.

Anonymous C unions use deterministic ordinal names such as
`ibv_wc::Anonymous`, `ibv_send_wr_1_0`, and `rdma_addr::Anonymous2`. The safe
`rdma-io` layer uses these generated names internally while preserving its
high-level API.
