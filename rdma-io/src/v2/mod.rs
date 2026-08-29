//! Ergonomic v2 RDMA API.
//!
//! This module provides a higher-level facade over the core RDMA primitives,
//! offering builder-driven resource setup, typed operations, and dual
//! CQ completion integration models for Rust async runtimes.
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
//! - **Dual CQ completion models**:
//!   - Direct CQ polling: [`Cq::poll()`] — explicit RDMA CQ polling
//!   - Fd/readiness-based: [`Cq::fd()`] + [`Completions`] — CQ completion
//!     channel fd registered with a Rust async runtime's reactor
//!
//! # Design
//!
//! The v2 API targets Rust async runtimes. It provides RDMA/CQ integration
//! primitives (fd exposure, cancellation-safe async CQ draining) without
//! implementing event-loop infrastructure, executors, or reactors.
//!
//! # Feature Flags
//!
//! - Core v2 types are always available (no feature required)
//! - `async` feature enables [`completion::Completions`] for async CQ notification
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
