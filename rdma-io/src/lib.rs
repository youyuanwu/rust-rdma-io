//! Safe Rust API for RDMA (libibverbs + librdmacm).
//!
//! Provides RAII wrappers for RDMA resources with `Arc`-based ownership
//! to enforce correct destruction order.

pub mod cm;
pub mod comp_channel;
pub mod cq;
pub mod device;
pub mod error;
pub mod mr;
pub mod pd;
pub mod qp;
pub mod stream;
pub mod wc;
pub mod wr;

#[cfg(any(feature = "tokio", feature = "smol"))]
pub mod async_cq;
#[cfg(any(feature = "tokio", feature = "smol"))]
pub mod async_qp;
#[cfg(feature = "tokio")]
pub mod async_stream;
#[cfg(feature = "tokio")]
pub mod tokio_notifier;

pub use error::{Error, Result};
