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
///
/// # Use case
///
/// Match typed resource, completion, transport, quarantine, and engine errors.
///
/// # Ownership and progress
///
/// Errors are owned values and do not drive progress or retain V1 errors.
///
/// # Safety and limits
///
/// Quarantine and wedge variants report retained ownership rather than
/// claiming unsafe cleanup succeeded.
///
/// # Availability
///
/// Available in every V2 feature profile.
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

    /// Connection close could not establish an exact-CQE or owning-QP
    /// destruction boundary for accepted WRs.
    ConnectionQuarantined {
        /// Accepted operations still awaiting a safe ownership boundary.
        outstanding_operations: usize,
        /// CQ admission credits retained with those operations.
        cq_debt: usize,
    },

    /// Connection retirement retained its complete resource bundle because
    /// the owning queue pair could not be destroyed.
    ConnectionDestroyQuarantined {
        /// Contextual provider destruction failure.
        cause: String,
    },

    /// Engine termination retained unsafe live state.
    EngineWedged {
        /// Complete QP/CM/MR/operation bundles retained fail-closed. This can
        /// be zero when only non-bundle CM routing work remains pending.
        retained_bundles: usize,
        /// Accepted operations still awaiting exact completions.
        outstanding_operations: usize,
        /// CQ admission credits retained with those operations.
        cq_debt: usize,
    },

    /// A wire-protocol violation was detected (bad magic, version, frame type,
    /// or payload length).
    ProtocolViolation(String),
}

impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Self::NoDevices => Self::NoDevices,
            Self::DeviceNotFound(name) => Self::DeviceNotFound(name.clone()),
            Self::Verbs(error) => Self::Verbs(clone_io_error(error)),
            Self::InvalidConfig(message) => Self::InvalidConfig(message.clone()),
            Self::PostFailed(error) => Self::PostFailed(clone_io_error(error)),
            Self::CompletionError { status, vendor_err } => Self::CompletionError {
                status: *status,
                vendor_err: *vendor_err,
            },
            Self::WouldBlock => Self::WouldBlock,
            Self::MessageTooLarge { size, capacity } => Self::MessageTooLarge {
                size: *size,
                capacity: *capacity,
            },
            Self::TransportClosed => Self::TransportClosed,
            Self::DriverShutdown => Self::DriverShutdown,
            Self::CapacityExhausted => Self::CapacityExhausted,
            Self::ConnectionQuarantined {
                outstanding_operations,
                cq_debt,
            } => Self::ConnectionQuarantined {
                outstanding_operations: *outstanding_operations,
                cq_debt: *cq_debt,
            },
            Self::ConnectionDestroyQuarantined { cause } => Self::ConnectionDestroyQuarantined {
                cause: cause.clone(),
            },
            Self::EngineWedged {
                retained_bundles,
                outstanding_operations,
                cq_debt,
            } => Self::EngineWedged {
                retained_bundles: *retained_bundles,
                outstanding_operations: *outstanding_operations,
                cq_debt: *cq_debt,
            },
            Self::ProtocolViolation(message) => Self::ProtocolViolation(message.clone()),
        }
    }
}

fn clone_io_error(error: &io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(code) => {
            let os_error = io::Error::from_raw_os_error(code);
            if os_error.kind() == error.kind() && os_error.to_string() == error.to_string() {
                os_error
            } else {
                io::Error::new(error.kind(), error.to_string())
            }
        }
        None => io::Error::new(error.kind(), error.to_string()),
    }
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
            Error::ConnectionQuarantined {
                outstanding_operations,
                cq_debt,
            } => write!(
                f,
                "connection quarantined with {outstanding_operations} outstanding operations and {cq_debt} retained CQ credits"
            ),
            Error::ConnectionDestroyQuarantined { cause } => {
                write!(f, "connection destroy quarantined: {cause}")
            }
            Error::EngineWedged {
                retained_bundles,
                outstanding_operations,
                cq_debt,
            } => write!(
                f,
                "engine wedged with {retained_bundles} retained bundles, {outstanding_operations} outstanding operations, and {cq_debt} retained CQ credits"
            ),
            Error::ProtocolViolation(detail) => {
                write!(f, "protocol violation: {detail}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Verbs(e) | Error::PostFailed(e) => Some(e),
            _ => None,
        }
    }
}

impl Error {
    pub(crate) fn from_v1(e: crate::Error) -> Self {
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
        let e = Error::from_v1(crate::Error::NoDevices);
        assert!(matches!(e, Error::NoDevices));

        let e = Error::from_v1(crate::Error::DeviceNotFound("test".into()));
        assert!(matches!(e, Error::DeviceNotFound(_)));

        let e = Error::from_v1(crate::Error::WouldBlock);
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
    }

    #[test]
    fn cloning_contextual_io_error_preserves_its_message() {
        let original = io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connect 192.0.2.1:7471: provider rejected route",
        );
        let cloned = clone_io_error(&original);
        assert_eq!(cloned.kind(), original.kind());
        assert_eq!(cloned.to_string(), original.to_string());

        let original = io::Error::from_raw_os_error(libc::ENOMEM);
        let cloned = clone_io_error(&original);
        assert_eq!(cloned.raw_os_error(), original.raw_os_error());
        assert_eq!(cloned.to_string(), original.to_string());
    }
}
