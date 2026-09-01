//! Typed V2 RDMA completions.

use crate::wc::{WcOpcode, WcStatus, WorkCompletion};

use super::error::{Error, Result};

/// A typed RDMA completion result.
///
/// # Use case
///
/// Use `Completion` directly as the element type for V2 direct, generic,
/// Tokio, and externally woken CQ polling.
///
/// # Ownership and progress
///
/// A completion is a copied provider result and owns no RDMA resource or
/// progress source.
///
/// # Safety and limits
///
/// Typed accessors expose correlation, QP identity, status, opcode, length,
/// and vendor error without exposing the raw V1 completion wrapper.
///
/// # Availability
///
/// Available in every V2 feature profile.
#[derive(Clone, Copy, Default)]
#[repr(transparent)]
pub struct Completion {
    wc: WorkCompletion,
}

impl Completion {
    /// The caller-chosen work-request correlation ID.
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

    /// Provider-reported queue-pair number.
    #[inline]
    pub fn qp_num(&self) -> u32 {
        self.wc.qp_num()
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

    pub(crate) fn from_raw(wc: WorkCompletion) -> Self {
        Self { wc }
    }

    pub(crate) fn into_raw(self) -> WorkCompletion {
        self.wc
    }

    pub(crate) fn raw_slice_mut(completions: &mut [Completion]) -> &mut [WorkCompletion] {
        unsafe { &mut *(completions as *mut [Completion] as *mut [WorkCompletion]) }
    }
}

impl std::fmt::Debug for Completion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completion")
            .field("wr_id", &self.wr_id())
            .field("status", &self.status())
            .field("opcode", &self.opcode())
            .field("qp_num", &self.qp_num())
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_defaults() {
        let cqe = Completion::default();
        assert_eq!(cqe.wr_id(), 0);
        assert_eq!(cqe.qp_num(), 0);
        assert!(cqe.is_success()); // default is status=0 = SUCCESS
        assert!(cqe.result().is_ok());
    }

    #[test]
    fn test_completion_debug() {
        let cqe = Completion::default();
        let debug = format!("{cqe:?}");
        assert!(debug.contains("Completion"));
        assert!(debug.contains("wr_id"));
    }
}
