//! Typed RDMA operations and completions.
//!
//! Provides an io_uring/compio-style submission/completion model for RDMA
//! operations. Operations are described as typed [`Op`] values with explicit
//! opcodes, submitted via [`Qp::submit()`](super::Qp::submit), and matched
//! to typed [`Completion`] results via correlation IDs.
//!
//! # Design
//!
//! This mirrors the SQE/CQE pattern from io_uring and the operation/completion
//! model from compio's IOCP integration:
//!
//! | RDMA v2 | io_uring | compio |
//! |---------|----------|--------|
//! | [`Op`] | SQE | `Op` trait impl |
//! | [`Completion`] | CQE | `OpCode::Output` |
//! | `wr_id` | `user_data` | token/key |
//! | [`OpCode`] | `opcode` | `OpCode` |
//!
//! # Example
//!
//! ```no_run
//! # use rdma_io::v2::*;
//! # use rdma_io::wc::WcOpcode;
//! # fn example(qp: &Qp, mr: &Mr, recv_mr: &mut Mr) -> Result<()> {
//! // Submit typed operations
//! qp.submit(Op::send(mr, 1))?;
//! qp.submit(Op::recv(recv_mr, 2))?;
//!
//! // Process typed completions
//! # let wc = rdma_io::wc::WorkCompletion::default();
//! let cqe = Completion::from(wc);
//! match cqe.opcode() {
//!     WcOpcode::Send => println!("send {} done", cqe.wr_id()),
//!     WcOpcode::Recv => println!("recv {} got {} bytes", cqe.wr_id(), cqe.byte_len()),
//!     _ => {}
//! }
//! cqe.result()?; // returns Err(CompletionError) on failure
//! # Ok(())
//! # }
//! ```

use crate::wc::{WcOpcode, WcStatus, WorkCompletion};

use super::error::{Error, Result};
use super::mr::{Mr, RemoteMr};

/// RDMA operation opcode for submission.
///
/// Mirrors io_uring's opcode field — each variant describes what the
/// hardware should do with the submitted buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCode {
    /// Two-sided send.
    Send,
    /// Two-sided receive (posts a receive buffer).
    Recv,
    /// One-sided RDMA Write to remote memory.
    Write,
    /// One-sided RDMA Read from remote memory.
    Read,
}

/// A typed RDMA operation for submission to a queue pair.
///
/// Analogous to an io_uring SQE or a compio operation — carries the
/// opcode, buffer references, and a caller-chosen correlation ID (`wr_id`)
/// that is returned in the [`Completion`].
///
/// Construct via the named constructors: [`Op::send()`], [`Op::recv()`],
/// [`Op::write()`], [`Op::read()`].
pub enum Op<'a> {
    /// Two-sided send: transmit the contents of `mr` to the connected peer.
    #[non_exhaustive]
    Send {
        /// Source buffer (immutable — data is read by the HCA).
        mr: &'a Mr,
        /// Caller-chosen correlation ID, returned in the completion.
        wr_id: u64,
    },

    /// Two-sided receive: post `mr` as a receive buffer for incoming data.
    #[non_exhaustive]
    Recv {
        /// Destination buffer (mutable — HCA writes incoming data here).
        mr: &'a mut Mr,
        /// Caller-chosen correlation ID, returned in the completion.
        wr_id: u64,
    },

    /// One-sided RDMA Write: copy data from `local` to `remote` without
    /// involving the remote CPU.
    #[non_exhaustive]
    Write {
        /// Local source buffer.
        local: &'a Mr,
        /// Remote destination descriptor (address + rkey + length).
        remote: &'a RemoteMr,
        /// Caller-chosen correlation ID, returned in the completion.
        wr_id: u64,
    },

    /// One-sided RDMA Read: copy data from `remote` into `local` without
    /// involving the remote CPU.
    #[non_exhaustive]
    Read {
        /// Local destination buffer (mutable — HCA writes remote data here).
        local: &'a mut Mr,
        /// Remote source descriptor (address + rkey + length).
        remote: &'a RemoteMr,
        /// Caller-chosen correlation ID, returned in the completion.
        wr_id: u64,
    },
}

impl<'a> Op<'a> {
    /// Create a send operation.
    pub fn send(mr: &'a Mr, wr_id: u64) -> Self {
        Op::Send { mr, wr_id }
    }

    /// Create a receive operation.
    pub fn recv(mr: &'a mut Mr, wr_id: u64) -> Self {
        Op::Recv { mr, wr_id }
    }

    /// Create an RDMA Write operation.
    pub fn write(local: &'a Mr, remote: &'a RemoteMr, wr_id: u64) -> Self {
        Op::Write {
            local,
            remote,
            wr_id,
        }
    }

    /// Create an RDMA Read operation.
    pub fn read(local: &'a mut Mr, remote: &'a RemoteMr, wr_id: u64) -> Self {
        Op::Read {
            local,
            remote,
            wr_id,
        }
    }

    /// The operation's opcode.
    pub fn opcode(&self) -> OpCode {
        match self {
            Op::Send { .. } => OpCode::Send,
            Op::Recv { .. } => OpCode::Recv,
            Op::Write { .. } => OpCode::Write,
            Op::Read { .. } => OpCode::Read,
        }
    }

    /// The operation's correlation ID.
    pub fn wr_id(&self) -> u64 {
        match self {
            Op::Send { wr_id, .. }
            | Op::Recv { wr_id, .. }
            | Op::Write { wr_id, .. }
            | Op::Read { wr_id, .. } => *wr_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Completion (CQE equivalent)
// ---------------------------------------------------------------------------

/// A typed RDMA completion result.
///
/// Analogous to an io_uring CQE or a compio completion — wraps a
/// [`WorkCompletion`] with typed accessors and a [`result()`](Completion::result)
/// method that converts failures to [`Error::CompletionError`].
///
/// Zero-cost: `Completion` is `#[repr(transparent)]` over `WorkCompletion`.
#[derive(Clone, Copy, Default)]
#[repr(transparent)]
pub struct Completion {
    wc: WorkCompletion,
}

impl Completion {
    /// The caller-chosen correlation ID from the submitted [`Op`].
    #[inline]
    pub fn wr_id(&self) -> u64 {
        self.wc.wr_id()
    }

    /// Whether the operation completed successfully.
    #[inline]
    pub fn is_success(&self) -> bool {
        self.wc.is_success()
    }

    /// The completion status.
    #[inline]
    pub fn status(&self) -> WcStatus {
        self.wc.status()
    }

    /// The completion opcode — identifies what operation completed.
    #[inline]
    pub fn opcode(&self) -> WcOpcode {
        self.wc.opcode()
    }

    /// Bytes transferred (meaningful for receive completions).
    #[inline]
    pub fn byte_len(&self) -> u32 {
        self.wc.byte_len()
    }

    /// Vendor-specific error information (meaningful when `!is_success()`).
    #[inline]
    pub fn vendor_err(&self) -> u32 {
        self.wc.vendor_err()
    }

    /// Check the completion status, returning `Ok(())` on success or
    /// `Err(Error::CompletionError { .. })` on failure.
    ///
    /// This is the primary way to handle completion results in the
    /// io_uring/compio style — check each CQE's result rather than
    /// inspecting raw status codes.
    pub fn result(&self) -> Result<()> {
        if self.wc.is_success() {
            Ok(())
        } else {
            Err(Error::CompletionError {
                status: self.wc.status(),
                vendor_err: self.wc.vendor_err(),
            })
        }
    }

    /// Access the underlying [`WorkCompletion`] for v1 interop.
    #[inline]
    pub fn as_wc(&self) -> &WorkCompletion {
        &self.wc
    }
}

impl From<WorkCompletion> for Completion {
    #[inline]
    fn from(wc: WorkCompletion) -> Self {
        Self { wc }
    }
}

impl AsRef<WorkCompletion> for Completion {
    #[inline]
    fn as_ref(&self) -> &WorkCompletion {
        &self.wc
    }
}

impl std::fmt::Debug for Completion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completion")
            .field("wr_id", &self.wr_id())
            .field("status", &self.status())
            .field("opcode", &self.opcode())
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

// Safety: Completion is #[repr(transparent)] over WorkCompletion,
// so &[WorkCompletion] can be safely viewed as &[Completion].
impl Completion {
    /// View a slice of [`WorkCompletion`]s as a slice of [`Completion`]s.
    ///
    /// Zero-cost conversion thanks to `#[repr(transparent)]`. Use this
    /// to get typed completions from [`Cq::poll()`](super::Cq::poll),
    /// [`CqPoller::poll_completions()`](super::CqPoller::poll_completions),
    /// or [`Completions::next()`](super::Completions::next).
    #[inline]
    pub fn from_wc_slice(wcs: &[WorkCompletion]) -> &[Completion] {
        // Safety: Completion is #[repr(transparent)] over WorkCompletion
        unsafe { &*(wcs as *const [WorkCompletion] as *const [Completion]) }
    }

    /// View a mutable slice of [`WorkCompletion`]s as [`Completion`]s.
    #[inline]
    pub fn from_wc_slice_mut(wcs: &mut [WorkCompletion]) -> &mut [Completion] {
        // Safety: Completion is #[repr(transparent)] over WorkCompletion
        unsafe { &mut *(wcs as *mut [WorkCompletion] as *mut [Completion]) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode_values() {
        assert_eq!(OpCode::Send, OpCode::Send);
        assert_ne!(OpCode::Send, OpCode::Recv);
        assert_ne!(OpCode::Write, OpCode::Read);
    }

    #[test]
    fn test_completion_from_wc() {
        let wc = WorkCompletion::default();
        let cqe = Completion::from(wc);
        assert_eq!(cqe.wr_id(), 0);
        assert!(cqe.is_success()); // default is status=0 = SUCCESS
        assert!(cqe.result().is_ok());
    }

    #[test]
    fn test_completion_slice_conversion() {
        let wcs = [WorkCompletion::default(); 4];
        let completions = Completion::from_wc_slice(&wcs);
        assert_eq!(completions.len(), 4);
        for cqe in completions {
            assert!(cqe.is_success());
        }
    }

    #[test]
    fn test_completion_debug() {
        let cqe = Completion::default();
        let debug = format!("{cqe:?}");
        assert!(debug.contains("Completion"));
        assert!(debug.contains("wr_id"));
    }
}
