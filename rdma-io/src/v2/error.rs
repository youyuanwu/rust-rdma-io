//! V2 error types for the ergonomic RDMA API.
//!
//! Provides typed error variants covering all failure modes in the v2 API,
//! including device discovery, resource configuration, operation posting,
//! and completion processing.

use std::fmt;
use std::io;

use crate::wc::WcStatus;

/// Result type alias for v2 operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by v2 RDMA operations.
///
/// Each variant corresponds to a distinct failure category, enabling
/// callers to handle errors with pattern matching rather than inspecting
/// opaque error codes.
#[derive(Debug)]
pub enum Error {
    /// No RDMA devices were found on this system.
    NoDevices,

    /// The requested RDMA device was not found by name.
    DeviceNotFound(String),

    /// An ibverbs or OS-level error occurred.
    Verbs(io::Error),

    /// A builder or configuration parameter was invalid.
    InvalidConfig(String),

    /// Posting a work request to a queue pair failed.
    PostFailed(io::Error),

    /// A work completion finished with an error status.
    CompletionError {
        /// The typed completion status code.
        status: WcStatus,
        /// Vendor-specific error information.
        vendor_err: u32,
    },

    /// A non-blocking operation found nothing ready (EAGAIN/EWOULDBLOCK).
    WouldBlock,

    /// An outbound message exceeds the configured buffer capacity.
    MessageTooLarge {
        /// The size of the message that was attempted.
        size: usize,
        /// The maximum allowed message size.
        capacity: usize,
    },

    /// The transport or connection has been shut down or disconnected.
    TransportClosed,

    /// The completion driver has stopped.
    DriverShutdown,

    /// The in-flight registry or buffer pool has no available capacity.
    CapacityExhausted,

    /// A wire-protocol violation was detected (bad magic, version, frame type,
    /// or payload length).
    ProtocolViolation(String),

    /// The transport has failed with a terminal error.
    ///
    /// Contains a [`TransportError`] snapshot describing the failure cause.
    /// Returned by frontend operations (`ready()`, `send()`, `recv()`) when
    /// the driver has exited with an error. Use [`super::message_transport::MessageTransport::error()`]
    /// for the same information.
    TransportFailed(TransportError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDevices => write!(f, "no RDMA devices found"),
            Error::DeviceNotFound(name) => write!(f, "device not found: {name}"),
            Error::Verbs(e) => write!(f, "verbs error: {e}"),
            Error::InvalidConfig(msg) => write!(f, "invalid configuration: {msg}"),
            Error::PostFailed(e) => write!(f, "post failed: {e}"),
            Error::CompletionError { status, vendor_err } => {
                write!(
                    f,
                    "completion error: {status:?} (vendor_err={vendor_err:#x})"
                )
            }
            Error::WouldBlock => write!(f, "operation would block"),
            Error::MessageTooLarge { size, capacity } => {
                write!(
                    f,
                    "message too large: {size} bytes exceeds {capacity} byte capacity"
                )
            }
            Error::TransportClosed => write!(f, "transport closed"),
            Error::DriverShutdown => write!(f, "driver shut down"),
            Error::CapacityExhausted => write!(f, "capacity exhausted"),
            Error::ProtocolViolation(detail) => {
                write!(f, "protocol violation: {detail}")
            }
            Error::TransportFailed(te) => write!(f, "transport failed: {te}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Verbs(e) | Error::PostFailed(e) => Some(e),
            Error::TransportFailed(te) => Some(te),
            _ => None,
        }
    }
}

impl From<crate::Error> for Error {
    fn from(e: crate::Error) -> Self {
        match e {
            crate::Error::NoDevices => Error::NoDevices,
            crate::Error::DeviceNotFound(name) => Error::DeviceNotFound(name),
            crate::Error::Verbs(io_err) => Error::Verbs(io_err),
            crate::Error::InvalidArg(msg) => Error::InvalidConfig(msg),
            crate::Error::WorkCompletion { status, vendor_err } => Error::CompletionError {
                status: WcStatus::from_raw(status),
                vendor_err,
            },
            crate::Error::WouldBlock => Error::WouldBlock,
            crate::Error::ConnectionFault(msg) => Error::Verbs(io::Error::other(msg)),
            crate::Error::Timeout(msg) => Error::Verbs(io::Error::other(msg)),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Verbs(e)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Transport Error (cloneable terminal error snapshot)
// ═══════════════════════════════════════════════════════════════════════════

/// Categories of terminal transport errors.
///
/// Provides typed failure classification for programmatic error handling.
/// Obtained from [`TransportError::kind()`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportErrorKind {
    /// HELLO handshake failed (timeout or validation error).
    ProtocolViolation,
    /// The driver was dropped or aborted without completing.
    DriverAborted,
    /// A completion queue driver error.
    CompletionError,
    /// An RDMA verbs or connection-level error.
    ConnectionError,
    /// The transport was shut down (clean or forced).
    Shutdown,
}

impl fmt::Display for TransportErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportErrorKind::ProtocolViolation => write!(f, "protocol violation"),
            TransportErrorKind::DriverAborted => write!(f, "driver aborted"),
            TransportErrorKind::CompletionError => write!(f, "completion error"),
            TransportErrorKind::ConnectionError => write!(f, "connection error"),
            TransportErrorKind::Shutdown => write!(f, "shutdown"),
        }
    }
}

/// A cloneable, thread-safe terminal error representation for frontend
/// inspection.
///
/// Created from the driver's terminal error and stored exactly once via
/// race-safe state transition semantics. Both the driver's
/// `Future<Output = Result<()>>` output and this frontend-observable snapshot
/// carry consistent cause information.
///
/// Obtained via [`super::message_transport::MessageTransport::error()`].
///
/// # Examples
///
/// ```no_run
/// # use rdma_io::v2::*;
/// # use rdma_io::v2::error::TransportErrorKind;
/// # async fn example(transport: MessageTransport) {
/// if let Some(err) = transport.error() {
///     match err.kind() {
///         TransportErrorKind::ProtocolViolation => {
///             eprintln!("handshake failed: {err}");
///         }
///         TransportErrorKind::DriverAborted => {
///             eprintln!("driver was dropped/aborted: {err}");
///         }
///         _ => eprintln!("transport error: {err}"),
///     }
/// }
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct TransportError {
    kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    /// Create a `TransportError` snapshot from a driver [`Error`].
    pub(crate) fn from_error(err: &Error) -> Self {
        let message = err.to_string();
        let kind = match err {
            Error::ProtocolViolation(_) => TransportErrorKind::ProtocolViolation,
            Error::DriverShutdown => TransportErrorKind::DriverAborted,
            Error::CompletionError { .. } => TransportErrorKind::CompletionError,
            Error::TransportClosed => TransportErrorKind::Shutdown,
            _ => TransportErrorKind::ConnectionError,
        };
        Self { kind, message }
    }

    /// The typed error category for programmatic matching.
    pub fn kind(&self) -> &TransportErrorKind {
        &self.kind
    }

    /// Human-readable error description preserving the original error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TransportError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let e = Error::NoDevices;
        assert_eq!(e.to_string(), "no RDMA devices found");

        let e = Error::DeviceNotFound("mlx5_0".into());
        assert!(e.to_string().contains("mlx5_0"));

        let e = Error::InvalidConfig("bad param".into());
        assert!(e.to_string().contains("bad param"));

        let e = Error::WouldBlock;
        assert_eq!(e.to_string(), "operation would block");
    }

    #[test]
    fn test_from_crate_error() {
        let e: Error = crate::Error::NoDevices.into();
        assert!(matches!(e, Error::NoDevices));

        let e: Error = crate::Error::DeviceNotFound("test".into()).into();
        assert!(matches!(e, Error::DeviceNotFound(_)));

        let e: Error = crate::Error::WouldBlock.into();
        assert!(matches!(e, Error::WouldBlock));
    }

    #[test]
    fn test_error_source() {
        use std::error::Error as StdError;

        let io_err = io::Error::other("test");
        let e = Error::Verbs(io_err);
        assert!(e.source().is_some());

        let e = Error::NoDevices;
        assert!(e.source().is_none());

        let te = TransportError::from_error(&Error::DriverShutdown);
        let e = Error::TransportFailed(te);
        assert!(e.source().is_some());
    }

    #[test]
    fn test_transport_error_from_error() {
        let te = TransportError::from_error(&Error::ProtocolViolation("HELLO timeout".into()));
        assert_eq!(*te.kind(), TransportErrorKind::ProtocolViolation);
        assert!(te.message().contains("HELLO timeout"));

        let te = TransportError::from_error(&Error::DriverShutdown);
        assert_eq!(*te.kind(), TransportErrorKind::DriverAborted);

        let te = TransportError::from_error(&Error::TransportClosed);
        assert_eq!(*te.kind(), TransportErrorKind::Shutdown);

        let te = TransportError::from_error(&Error::CompletionError {
            status: WcStatus::from_raw(5),
            vendor_err: 0,
        });
        assert_eq!(*te.kind(), TransportErrorKind::CompletionError);

        let te = TransportError::from_error(&Error::Verbs(io::Error::other("test")));
        assert_eq!(*te.kind(), TransportErrorKind::ConnectionError);
    }

    #[test]
    fn test_transport_error_clone() {
        let te = TransportError::from_error(&Error::DriverShutdown);
        let te2 = te.clone();
        assert_eq!(*te.kind(), *te2.kind());
        assert_eq!(te.message(), te2.message());
    }

    #[test]
    fn test_transport_error_display() {
        let te = TransportError::from_error(&Error::ProtocolViolation("bad magic".into()));
        let display = te.to_string();
        assert!(display.contains("bad magic"));
    }

    #[test]
    fn test_transport_failed_display() {
        let te = TransportError::from_error(&Error::DriverShutdown);
        let e = Error::TransportFailed(te);
        assert!(e.to_string().contains("transport failed"));
    }
}
