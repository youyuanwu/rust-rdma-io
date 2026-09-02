//! V2 memory registration with access-intent semantics.
//!
//! Provides [`AccessIntent`] for declaring memory access requirements
//! in domain terms rather than raw flag combinations, and [`Mr`] / [`RemoteMr`]
//! wrappers with ergonomic accessors.

use super::error::{Error, Result};
use super::pd::Pd;

use crate::mr::{AccessFlags, OwnedMemoryRegion};

/// Declares the intended access pattern for a memory registration.
///
/// Maps to the appropriate combination of RDMA access flags:
///
/// | Intent | Flags |
/// |--------|-------|
/// | `LocalOnly` | `LOCAL_WRITE` |
/// | `RemoteRead` | `LOCAL_WRITE \| REMOTE_READ` |
/// | `RemoteWrite` | `LOCAL_WRITE \| REMOTE_WRITE` |
/// | `RemoteReadWrite` | `LOCAL_WRITE \| REMOTE_READ \| REMOTE_WRITE` |
///
/// # Use case
///
/// Select domain-level registration intent without composing raw flags.
///
/// # Ownership and progress
///
/// The value owns no resource and creates no progress source.
///
/// # Safety and limits
///
/// Raw `AccessFlags` conversion remains an implementation detail.
///
/// # Availability
///
/// Available in every V2 feature profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessIntent {
    /// Local read/write only. Cannot be accessed remotely.
    LocalOnly,
    /// Allows remote RDMA Read operations.
    RemoteRead,
    /// Allows remote RDMA Write operations.
    RemoteWrite,
    /// Allows both remote RDMA Read and Write operations.
    RemoteReadWrite,
}

impl AccessIntent {
    pub(crate) fn to_flags(self) -> AccessFlags {
        match self {
            AccessIntent::LocalOnly => AccessFlags::LOCAL_WRITE,
            AccessIntent::RemoteRead => AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ,
            AccessIntent::RemoteWrite => AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE,
            AccessIntent::RemoteReadWrite => {
                AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ | AccessFlags::REMOTE_WRITE
            }
        }
    }
}

/// A registered memory region with owned buffer.
///
/// # Use case
///
/// Own a registered local buffer for named QP or engine operations.
///
/// # Ownership and progress
///
/// The MR owns its buffer and retains its protection domain. It owns no task.
///
/// # Safety and limits
///
/// Deregistration occurs on drop only after operation ownership returns it.
/// No raw V1 memory-region accessor is exposed.
///
/// # Availability
///
/// Created through [`Pd::reg_mr`].
pub struct Mr {
    inner: OwnedMemoryRegion,
}

impl Mr {
    /// The local key for posting work requests.
    pub fn lkey(&self) -> u32 {
        self.inner.lkey()
    }

    /// The remote key (valid only if registered with remote access).
    pub fn rkey(&self) -> u32 {
        self.inner.rkey()
    }

    /// The registered address as a u64 (for work request construction).
    pub fn addr(&self) -> u64 {
        self.inner.addr()
    }

    /// The length of the registered buffer in bytes.
    pub fn len(&self) -> usize {
        self.inner.as_slice().len()
    }

    /// Returns true if the registered buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read access to the registered buffer.
    pub fn as_slice(&self) -> &[u8] {
        self.inner.as_slice()
    }

    /// Mutable access to the registered buffer.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.inner.as_mut_slice()
    }

    /// Create a remote memory descriptor for one-sided operations.
    ///
    /// Send this descriptor to a peer (e.g., via SEND/RECV) so they
    /// can perform RDMA Read/Write against this buffer.
    pub fn to_remote(&self) -> RemoteMr {
        let r = self.inner.to_remote();
        RemoteMr {
            addr: r.addr,
            rkey: r.rkey,
            len: r.len,
        }
    }
}

/// Descriptor for a remote peer's registered memory.
///
/// Contains the address, key, and length needed for one-sided
/// RDMA Read/Write operations against a remote buffer.
///
/// Typically obtained by receiving a [`Mr::to_remote()`] descriptor
/// from the remote peer via a SEND/RECV exchange.
///
/// # Use case
///
/// Describe a peer buffer for named RDMA read and write operations.
///
/// # Ownership and progress
///
/// This copied descriptor owns no local registration or progress source.
///
/// # Safety and limits
///
/// The peer controls validity and lifetime; V1 conversion helpers are absent.
///
/// # Availability
///
/// Available whenever the peer communicates address, key, and length.
#[derive(Debug, Clone, Copy)]
pub struct RemoteMr {
    /// Remote virtual address.
    pub addr: u64,
    /// Remote key for access authorization.
    pub rkey: u32,
    /// Length of the remote buffer in bytes.
    pub len: u32,
}

impl Pd {
    /// Register a new memory region with the specified access intent.
    ///
    /// Allocates a buffer of `size` bytes (zero-initialized) and registers
    /// it with the RDMA device. The access flags are determined by the
    /// [`AccessIntent`].
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if `size` is 0
    /// - [`Error::Verbs`] if memory registration fails
    pub fn reg_mr(&self, size: usize, access: AccessIntent) -> Result<Mr> {
        if size == 0 {
            return Err(Error::InvalidConfig(
                "memory region size must be > 0".into(),
            ));
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(size)
            .map_err(|_| Error::InvalidConfig("memory region allocation failed".into()))?;
        buf.resize(size, 0);
        let flags = access.to_flags();
        let omr = self
            .raw_pd()
            .reg_mr_owned(buf, flags)
            .map_err(Error::from_v1)?;
        Ok(Mr { inner: omr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_access_intent_to_flags() {
        assert_eq!(AccessIntent::LocalOnly.to_flags(), AccessFlags::LOCAL_WRITE);
        assert_eq!(
            AccessIntent::RemoteRead.to_flags(),
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ
        );
        assert_eq!(
            AccessIntent::RemoteWrite.to_flags(),
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE
        );
        assert_eq!(
            AccessIntent::RemoteReadWrite.to_flags(),
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ | AccessFlags::REMOTE_WRITE
        );
    }
}
