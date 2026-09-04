//! Low-level operation/completion state composed by the v2 engine.

use std::collections::VecDeque;
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "test-hooks"))]
use super::operation::CqeReject;
use super::operation::{CqCreditPool, OperationRegistry};
use super::registry::ConnectionToken;
use crate::v2::error::Result;
use crate::v2::qp::BatchPostOutcome;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

/// Posting-only QP authority supplied by the session layer.
///
/// This boundary deliberately excludes QP error transitions, destruction,
/// disconnect, CM ownership, and retirement.
pub(super) trait IoPostAuthority: Send + Sync {
    fn qp_num(&self) -> u32;
    fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome>;
    fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome>;
}

/// Immutable session identity accepted by the operation/completion core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EstablishedIoIdentity {
    pub(super) connection: ConnectionToken,
    pub(super) qp_num: u32,
}

/// Opaque posting capability for one established session connection.
///
/// The concrete authority held here contains only a weak reference to the
/// session-owned resource bundle.
pub(super) struct EstablishedIoConnection {
    identity: EstablishedIoIdentity,
    poster: Arc<dyn IoPostAuthority>,
}

impl EstablishedIoConnection {
    pub(super) fn new(
        identity: EstablishedIoIdentity,
        poster: Arc<dyn IoPostAuthority>,
    ) -> Arc<Self> {
        debug_assert_eq!(identity.qp_num, poster.qp_num());
        Arc::new(Self { identity, poster })
    }

    pub(super) fn identity(&self) -> EstablishedIoIdentity {
        self.identity
    }

    pub(super) fn post_send(&self, batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        self.poster.post_send(batch)
    }

    pub(super) fn post_recv(&self, batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        self.poster.post_recv(batch)
    }
}

/// State owned by the low-level operation/completion runtime.
///
/// The fields remain visible to the pre-extraction `operation` module during
/// the first migration phase. The completed extraction moves those consumers
/// behind focused methods on this type.
pub(super) struct IoCore {
    pub(super) operations: OperationRegistry,
    pub(super) cq_credits: CqCreditPool,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqes: AtomicU64,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) rejected_cqe_reasons: Mutex<Vec<CqeReject>>,
    pub(super) accepted_operations: AtomicUsize,
    pub(super) pending_reclamations: AtomicUsize,
    pub(super) quarantined_operations: AtomicUsize,
    pub(super) quarantined_mrs: AtomicUsize,
    pub(super) quarantined_bytes: AtomicUsize,
    pub(super) published_completion_connections: Mutex<VecDeque<ConnectionToken>>,
}

impl IoCore {
    pub(super) fn new(max_inflight_operations: usize, cq_capacity: usize) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            operations: OperationRegistry::new(max_inflight_operations)?,
            cq_credits: CqCreditPool::new(cq_capacity),
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqes: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-hooks"))]
            rejected_cqe_reasons: Mutex::new(Vec::new()),
            accepted_operations: AtomicUsize::new(0),
            pending_reclamations: AtomicUsize::new(0),
            quarantined_operations: AtomicUsize::new(0),
            quarantined_mrs: AtomicUsize::new(0),
            quarantined_bytes: AtomicUsize::new(0),
            published_completion_connections: Mutex::new(VecDeque::new()),
        }))
    }
}
