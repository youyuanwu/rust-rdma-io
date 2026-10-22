//! Tokio CQ notifier using `AsyncFd`.

use std::future::Future;
use std::io;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;

use crate::async_cq::CqNotifier;

/// Tokio adapter for CQ fd readiness notification.
///
/// Wraps the completion channel fd in a tokio `AsyncFd` so the
/// async reactor (epoll) can wake the task when a CQ event arrives.
pub struct TokioCqNotifier {
    async_fd: AsyncFd<OwnedFd>,
}

impl TokioCqNotifier {
    /// Create a new notifier owning a duplicate of the given fd.
    ///
    /// The fd must already be set to non-blocking mode.
    pub fn new(fd: RawFd) -> io::Result<Self> {
        // Duplicate without borrowing an unvalidated raw fd; invalid fds return EBADF.
        let duplicated_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl returned a new, uniquely owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(duplicated_fd) };
        // SAFETY: AsyncFd owns the duplicate and never replaces or closes it early.
        Ok(Self {
            async_fd: unsafe { AsyncFd::register(fd)? },
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

    fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.async_fd.poll_read_ready(cx) {
            Poll::Ready(Ok(mut guard)) => {
                guard.clear_ready();
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[tokio::test]
    async fn notifier_keeps_descriptor_alive_after_original_is_dropped() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let notifier = TokioCqNotifier::new(reader.as_raw_fd()).unwrap();
        drop(reader);

        writer.write_all(b"ready").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), notifier.readable())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn invalid_descriptor_returns_io_error() {
        let error = TokioCqNotifier::new(-1).err().unwrap();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
