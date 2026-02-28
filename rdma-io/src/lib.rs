//! Safe Rust API for RDMA (libibverbs + librdmacm).
//!
//! Provides RAII wrappers for RDMA resources with `Arc`-based ownership
//! to enforce correct destruction order.

pub mod cq;
pub mod device;
pub mod error;
pub mod mr;
pub mod pd;
pub mod qp;
pub mod wc;
pub mod wr;

pub use error::{Error, Result};
