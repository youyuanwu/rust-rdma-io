//! Tokio CQ notifier using `AsyncFd`.

use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;

use tokio::io::unix::AsyncFd;

use crate::async_cq::CqNotifier;

/// Tokio adapter for CQ fd readiness notification.
///
/// Wraps the completion channel fd in a tokio `AsyncFd` so the
/// async reactor (epoll) can wake the task when a CQ event arrives.
pub struct TokioCqNotifier {
    async_fd: AsyncFd<RawFd>,
}

impl TokioCqNotifier {
    /// Create a new notifier for the given fd.
    ///
    /// The fd must already be set to non-blocking mode.
    pub fn new(fd: RawFd) -> io::Result<Self> {
        Ok(Self {
            async_fd: AsyncFd::new(fd)?,
        })
    }
}

impl CqNotifier for TokioCqNotifier {
    fn readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async {
            let mut guard = self.async_fd.readable().await?;
            guard.clear_ready();
            Ok(())
        })
    }
}
