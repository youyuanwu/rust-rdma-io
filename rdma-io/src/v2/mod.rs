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
//! - **Dual CQ completion models** (both async-native):
//!   - Fd/readiness-based: [`Completions`] — CQ completion channel fd
//!     registered with async runtime reactor, arm-drain pattern
//!   - CQ polling-based: [`CqPoller`] — direct RDMA CQ polling with
//!     smoltcp-style waker registration for async runtime integration
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
pub mod op;
pub mod pd;
pub mod qp;

#[cfg(feature = "async")]
pub mod completion;
#[cfg(feature = "tokio")]
pub mod connection;
#[cfg(feature = "async")]
pub mod cq_poller;
#[cfg(feature = "tokio")]
pub mod driver;
#[cfg(feature = "async")]
pub mod inflight;
#[cfg(feature = "tokio")]
pub mod message_transport;
pub mod protocol;
#[cfg(feature = "tokio")]
pub mod shared_qp;
#[cfg(feature = "tokio")]
mod tokio_support;

// Re-export primary types at v2 level.
pub use context::Context;
pub use cq::{Cq, CqBuilder};
pub use error::{Error, Result};
pub use mr::{AccessIntent, Mr, RemoteMr};
pub use op::{Completion, Op, OpCode};
pub use pd::Pd;
pub use qp::{Qp, QpBuilder};

#[cfg(feature = "async")]
pub use completion::Completions;

#[cfg(feature = "async")]
pub use cq_poller::CqPoller;

#[cfg(feature = "async")]
pub use crate::async_cq::CqNotifier;

#[cfg(feature = "tokio")]
pub use driver::{CqDriverHandle, FdCqDriver, PollingCqDriver};

#[cfg(feature = "tokio")]
pub use shared_qp::{OpFuture, SharedQp};

#[cfg(feature = "tokio")]
pub use connection::{CompletionMode, Connection};

#[cfg(feature = "tokio")]
pub use message_transport::{MessageTransport, MessageTransportBuilder, MessageTransportDriver, ReceivedMessage};

#[cfg(feature = "tokio")]
pub use tokio_support::TokioCompletions;
