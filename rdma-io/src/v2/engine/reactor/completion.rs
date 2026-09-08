//! Resource-free typed completion storage for admitted engine commands.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Waker;

use futures_util::task::AtomicWaker;

use super::super::lifecycle::TakeOnceResult;
use super::super::registry::lock_unpoison;
use crate::v2::error::{Error, Result};

/// Take-once command result with cancellation and register/recheck support.
///
/// Backend ownership is intentionally absent. Losing this frontend observer
/// can suppress delivery, but cannot release provider-visible state.
pub(in crate::v2::engine) struct CommandCompletion<T> {
    result: Mutex<TakeOnceResult<T>>,
    cancelled: AtomicBool,
    waker: AtomicWaker,
}

impl<T> CommandCompletion<T> {
    pub(in crate::v2::engine) fn new() -> Self {
        Self {
            result: Mutex::new(TakeOnceResult::Pending),
            cancelled: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    pub(in crate::v2::engine) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(in crate::v2::engine) fn complete(&self, result: Result<T>) {
        let mut current = lock_unpoison(&self.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(result);
            drop(current);
            self.waker.wake();
        }
    }

    pub(in crate::v2::engine) fn take_result(&self) -> Option<Result<T>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Ready(result) => Some(result),
            TakeOnceResult::Pending => {
                *current = TakeOnceResult::Pending;
                None
            }
            TakeOnceResult::Taken => None,
        }
    }

    pub(in crate::v2::engine) fn register(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    /// Cancel delivery while preserving a backend error that already won.
    ///
    /// A successfully completed value that was never observed is returned to
    /// the caller for normal drop/close handling.
    pub(in crate::v2::engine) fn cancel(&self, error: Error) -> Option<T> {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let (replacement, undelivered) =
            match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
                TakeOnceResult::Pending => (TakeOnceResult::Ready(Err(error)), None),
                TakeOnceResult::Ready(Ok(value)) => {
                    (TakeOnceResult::Ready(Err(error)), Some(value))
                }
                TakeOnceResult::Ready(Err(existing)) => {
                    (TakeOnceResult::Ready(Err(existing)), None)
                }
                TakeOnceResult::Taken => (TakeOnceResult::Taken, None),
            };
        *current = replacement;
        drop(current);
        self.waker.wake();
        undelivered
    }
}

#[cfg(test)]
mod tests {
    use super::CommandCompletion;
    use crate::v2::error::Error;

    #[test]
    fn completion_is_take_once_and_recheck_safe() {
        let completion = CommandCompletion::new();
        completion.complete(Ok(7usize));
        completion.register(&futures_util::task::noop_waker());
        assert_eq!(completion.take_result().unwrap().unwrap(), 7);
        assert!(completion.take_result().is_none());
    }

    #[test]
    fn cancellation_preserves_an_existing_backend_error() {
        let completion = CommandCompletion::<usize>::new();
        completion.complete(Err(Error::TransportClosed));
        assert!(completion.cancel(Error::DriverShutdown).is_none());
        assert!(matches!(
            completion.take_result(),
            Some(Err(Error::TransportClosed))
        ));
        assert!(completion.is_cancelled());
    }

    #[test]
    fn cancellation_returns_an_undelivered_success() {
        let completion = CommandCompletion::new();
        completion.complete(Ok(9usize));
        assert_eq!(completion.cancel(Error::DriverShutdown), Some(9));
        assert!(matches!(
            completion.take_result(),
            Some(Err(Error::DriverShutdown))
        ));
    }
}
