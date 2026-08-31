//! Ergonomic RDMA resources plus one explicitly driven shared runtime engine.
//!
//! With the `tokio` feature, [`RdmaEngineBuilder::build`] returns
//! ([`RdmaEngine`], [`RdmaEngineDriver`]). The handle submits connection,
//! listener, operation, message, and lifecycle work; the driver is the sole
//! CQ/CM consumer. This resembles the ownership split of an io_uring instance
//! or IOCP completion port, although the implementation uses libibverbs and
//! librdmacm directly.
//!
//! The library creates no task or thread. Applications must spawn or directly
//! poll the one driver future, and no engine work progresses otherwise.
//! Message transport adds zero tasks: receive completions, reposts, DATA,
//! CREDIT, HELLO, disconnect handling, and reclamation all run as bounded
//! engine-driver work.
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
//! [`Cq`], [`Mr`], [`Qp`], typed [`Op`] values, [`Completions`], and
//! [`CqPoller`]. V1 APIs are separate and unchanged; existing v2 endpoint
//! compatibility is not provided.
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
#[cfg(feature = "async")]
pub mod cq_poller;
#[cfg(feature = "tokio")]
pub mod engine;
#[cfg(feature = "tokio")]
pub mod message_transport;
pub mod protocol;
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
pub use engine::{
    CompletionMode, RdmaConnection, RdmaConnectionConfig, RdmaConnectionDiagnostics,
    RdmaConnectionIdentity, RdmaEngine, RdmaEngineBuilder, RdmaEngineDiagnostics, RdmaEngineDriver,
    RdmaEngineLifecycle, RdmaEngineTerminalError, RdmaListener, RdmaListenerConfig,
    RdmaListenerDiagnostics, RdmaOperation,
};

#[cfg(feature = "tokio")]
pub use message_transport::{MessageTransport, MessageTransportBuilder, ReceivedMessage};

#[cfg(feature = "tokio")]
pub use tokio_support::TokioCompletions;
