//! Ergonomic RDMA resources plus one explicitly driven shared runtime engine.
//!
//! With the `tokio` feature, [`RdmaEngineBuilder::build`] returns
//! ([`RdmaEngine`], [`RdmaEngineDriver`]). The handle submits connection,
//! listener, operation, and lifecycle work; the driver is the sole CQ/CM
//! consumer. Message protocol work has a separate per-connection driver. This
//! resembles the ownership split of an io_uring instance
//! or IOCP completion port, although the implementation uses libibverbs and
//! librdmacm directly.
//!
//! # Use case
//!
//! Import retained production types from `rdma_io::v2`; implementation modules
//! are private and each item has one public spelling.
//!
//! # Ownership and progress
//!
//! Independent resources are caller-driven. Engine resources progress only
//! while the returned [`RdmaEngineDriver`] is polled.
//!
//! # Safety and limits
//!
//! V2 exposes typed ownership and completion APIs without raw V1 resource
//! adoption, borrowed contexts, or raw completion buffers.
//!
//! # Availability
//!
//! Core resources are always available; async and engine APIs follow the
//! feature flags documented below.
//!
//! The library creates no task or thread. Applications must spawn or directly
//! poll the engine driver, and must also poll the [`MessageTransportDriver`]
//! returned for every message connection. CQ/CM routing, exact completion
//! dispatch, reclamation, and safe teardown remain engine work. HELLO, DATA,
//! CREDIT, receive reposting, fairness, and message lifecycle are bounded
//! connection-driver work.
//!
//! ```no_run
//! # use rdma_io::v2::*;
//! # async fn example() -> Result<()> {
//! let (engine, driver) = RdmaEngineBuilder::new("rxe0").build()?;
//! let driver_task = tokio::spawn(driver);
//! let connection = engine.connect("192.0.2.1:7471".parse().unwrap()).await?;
//! connection.close().await?;
//! engine.shutdown().await?;
//! driver_task.await.expect("engine driver panicked")?;
//! # Ok(())
//! # }
//! ```
//!
//! Readiness is the default completion mode and requires active Tokio I/O
//! during `build()`. Polling mode may be built outside a runtime because it
//! allocates no CQ notification channel, but every driver poll must occur in
//! an active Tokio runtime with time enabled before deadlines are armed.
//!
//! The retained independent resource surface includes [`Context`], [`Pd`],
//! [`Cq`], [`Mr`], [`Qp`], the four named [`Qp::post_send`],
//! [`Qp::post_recv`], [`Qp::post_write`], and [`Qp::post_read`] operations,
//! [`Completions`], and [`CqPoller`]. V1 APIs are separate and unchanged.
//!
//! # Feature Flags
//!
//! - Core v2 types are always available (no feature required)
//! - `async` feature enables [`Completions`] for async CQ notification
//! - `tokio` feature adds [`Cq::completions_tokio()`] convenience

#![deny(missing_docs)]

#[cfg(feature = "async")]
mod completion;
mod context;
mod cq;
#[cfg(feature = "async")]
mod cq_poller;
#[cfg(feature = "tokio")]
mod engine;
mod error;
#[cfg(feature = "tokio")]
mod message_transport;
mod mr;
mod op;
mod pd;
#[cfg(any(test, feature = "tokio"))]
mod protocol;
mod qp;
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_support;
#[cfg(feature = "tokio")]
mod tokio_support;

// Re-export primary types at v2 level.
pub use context::Context;
pub use cq::{Cq, CqBuilder};
pub use error::{Error, Result};
pub use mr::{AccessIntent, Mr, RemoteMr};
pub use op::Completion;
pub use pd::Pd;
pub use qp::{Qp, QpBuilder};

#[cfg(feature = "async")]
pub use completion::Completions;

#[cfg(feature = "async")]
pub use cq_poller::CqPoller;

#[cfg(feature = "async")]
pub use crate::async_cq::CqNotifier;

#[cfg(feature = "tokio")]
pub use engine::{
    CompletionMode, RdmaConnection, RdmaConnectionConfig, RdmaConnectionIdentity, RdmaEngine,
    RdmaEngineBuilder, RdmaEngineDiagnostics, RdmaEngineDriver, RdmaEngineLifecycle,
    RdmaEngineTerminalError, RdmaListener, RdmaListenerConfig, RdmaOperation,
};

#[cfg(feature = "tokio")]
pub use message_transport::{
    MessageTransport, MessageTransportBuilder, MessageTransportDriver, ReceivedMessage,
};

#[cfg(feature = "tokio")]
pub use tokio_support::TokioCompletions;
