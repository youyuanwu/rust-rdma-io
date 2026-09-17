# RDMA Bindings via bnd

`rdma-io-sys` uses the bnd 0.0.7 direct-Clang toolchain to generate checked-in
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
   canonical bnd-rdma.winmd
             |
      metadata remapping
             |
             v
         bnd-bindgen
             |
             v
 rdma-io-sys/src/rdma/*
```

The generator parses one coherent C translation unit and emits RDL grouped by
the header that defines each item. It then compiles the RDL into canonical
WinMD metadata and temporarily remaps declarations into the stable public
modules:

- `rdma::ibverbs`
- `rdma::rdmacm`
- `rdma::wrapper`

The remapped metadata is only a code-generation input. The canonical metadata
is checked in at `rdma-io-sys/winmd/bnd-rdma.winmd`.

## Toolchain

The generator uses the published bnd 0.0.7 crates:

```toml
bnd-clang = "0.0.7"
bnd-bindgen = "0.0.7"
bnd-linux = "0.0.7"
bnd-macros = "0.0.6"
windows-metadata = "0.100"
windows-rdl = { version = "0.100", default-features = false }
```

`bnd-clang` and `bnd-bindgen` retain the Rust library names
`windows_clang` and `windows_bindgen`.

## Header ownership

`bnd-rdma-gen/src/clang.rs` defines the generation inputs and routes:

| Headers | Rust module | Native library |
|---|---|---|
| `infiniband/verbs.h`, verbs UAPI dependencies | `rdma::ibverbs` | `ibverbs` |
| `rdma/rdma_cma.h`, `infiniband/sa.h` | `rdma::rdmacm` | `rdmacm` |
| `rdma-io-sys/wrapper/wrapper.h` | `rdma::wrapper` | `rdma_wrapper` |

All headers are parsed together so shared RDMA declarations have one owner and
cross-module references remain consistent.

## Shared Linux types

The bnd 0.0.7 Linux bindings use defining-header modules directly below
`bnd_linux::libc`. Examples include:

| C type | Rust path |
|---|---|
| `__be16`, `__be32`, `__be64`, `ssize_t` | `bnd_linux::libc::types::*` |
| `pthread_mutex_t`, `pthread_cond_t` | `bnd_linux::libc::pthreadtypes::*` |
| `sockaddr`, `sockaddr_storage` | `bnd_linux::libc::socket::*` |
| `sockaddr_in`, `sockaddr_in6` | `bnd_linux::libc::r#in::*` |
| `socklen_t` | `bnd_linux::libc::unistd::socklen_t` |
| `timespec` | `bnd_linux::libc::struct_timespec::timespec` |

The generator detects names shared by the RDMA and Linux metadata, moves those
types to the temporary `libc` namespace, and configures exact
`bnd-bindgen::Bindgen::external_reference` routes. The synthetic `Apis`
container is explicitly excluded from this matching because each metadata file
uses it independently for native functions.

This late ownership assignment is intentional. Passing the full Linux WinMD to
the current bnd-clang scrape drops RDMA structs that embed referenced socket
types; parsing the complete RDMA surface first preserves those ABI layouts,
after which metadata remapping gives the generated Rust code the correct
`bnd-linux` type identity.

## Inline function wrappers

libibverbs exposes many operations as `static inline` functions, which cannot
be linked directly from Rust. `rdma-io-sys/wrapper/wrapper.c` provides exported
`rdma_wrap_*` functions, and `wrapper.h` is included in the same generation
translation unit. This keeps wrapper signatures tied to the generated
ibverbs types.

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

The recipe downloads the canonical `bnd-linux` 0.0.7 WinMD from the bnd
publication commit, verifies its SHA-256 digest, and runs `bnd-rdma-gen` with
one Cargo build job.

Generation updates:

- `rdma-io-sys/winmd/bnd-rdma.winmd`
- `rdma-io-sys/src/rdma/mod.rs`
- `rdma-io-sys/src/rdma/ibverbs/mod.rs`
- `rdma-io-sys/src/rdma/rdmacm/mod.rs`
- `rdma-io-sys/src/rdma/wrapper/mod.rs`
- generated feature dependencies below the marker in
  `rdma-io-sys/Cargo.toml`

Do not edit generated files directly.

## ABI and source compatibility

The 0.0.7 pipeline derives native integer widths from Clang. C `size_t`
parameters therefore generate as Rust `usize` rather than `u64`.

Anonymous C unions now use deterministic ordinal names such as
`ibv_wc::Anonymous`, `ibv_send_wr_1_0`, and `rdma_addr::Anonymous2`.
The safe `rdma-io` layer uses these generated names internally while preserving
the existing high-level API.
