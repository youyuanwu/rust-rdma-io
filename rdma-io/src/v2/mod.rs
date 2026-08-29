//! Ergonomic v2 RDMA API.
//!
//! This module provides a higher-level facade over the core RDMA primitives,
//! offering builder-driven resource setup, typed operations, and dual
//! completion models (readiness-based and polling-based).
//!
//! # Overview
//!
//! The v2 API reduces RDMA setup complexity from ~8 manual steps to a
//! guided builder flow. It supports:
//!
//! - **Device discovery**: [`Context::open_first()`] and [`Context::open_by_name()`]
//! - **Resource builders**: [`CqBuilder`] for completion queues, [`QpBuilder`]
//!   for queue pairs
//! - **Memory registration**: [`Mr`] with [`AccessIntent`] for clear access semantics
//! - **Typed operations**: [`Qp::post_send()`], [`Qp::post_recv()`],
//!   [`Qp::post_write()`], [`Qp::post_read()`]
//! - **Dual completion models**:
//!   - Polling: [`Cq::poll()`] for busy-loop or custom event loops
//!   - Readiness: [`Cq::fd()`] for event-loop registration
//!
//! # Feature Flags
//!
//! - Core v2 types are always available (no feature required)
//! - `async` feature enables [`completion::Completions`] for async completion awaiting
//! - `tokio` feature adds [`Cq::completions_tokio()`] convenience

pub mod context;
pub mod cq;
pub mod error;
pub mod mr;
pub mod pd;
pub mod qp;

#[cfg(feature = "async")]
pub mod completion;
#[cfg(feature = "tokio")]
mod tokio_support;

// Re-export primary types at v2 level.
pub use context::Context;
pub use cq::{Cq, CqBuilder};
pub use error::{Error, Result};
pub use mr::{AccessIntent, Mr, RemoteMr};
pub use pd::Pd;
pub use qp::{Qp, QpBuilder};

#[cfg(feature = "async")]
pub use completion::Completions;

#[cfg(feature = "async")]
pub use crate::async_cq::CqNotifier;

#[cfg(feature = "tokio")]
pub use tokio_support::TokioCompletions;
