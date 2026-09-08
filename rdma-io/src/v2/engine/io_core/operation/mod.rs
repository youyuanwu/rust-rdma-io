//! Owned low-level operation futures, admission, and exact CQE routing.

mod accounting;
mod batch;
mod completion;
mod effects;
pub(in crate::v2::engine) mod future;
mod reclamation;
mod state;
#[cfg(test)]
mod test_support;
mod validation;

#[cfg(test)]
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    sync::Mutex,
    sync::atomic::Ordering,
    sync::atomic::{AtomicBool, AtomicUsize},
    task::{Context, Poll},
};

#[cfg(test)]
use super::super::io::{
    IoEventDestination, IoOperationContext, IoRecvRequest, IoSendRequest, IoSubmissionDisposition,
};
#[cfg(test)]
use super::super::registry::lock_unpoison;
#[cfg(test)]
use super::super::registry::{Lookup, OperationToken};
#[cfg(test)]
use super::Direction;
#[cfg(test)]
use super::IoCore;
#[cfg(test)]
use super::OperationKind;
#[cfg(test)]
use crate::v2::error::Error;
#[cfg(test)]
use crate::v2::error::Result;
#[cfg(test)]
use crate::v2::mr::Mr;
#[cfg(test)]
use crate::v2::op::Completion;
#[cfg(test)]
use crate::v2::qp::BatchPostOutcome;
#[cfg(test)]
use crate::wc::WcOpcode;
#[cfg(test)]
use crate::wc::WorkCompletion;
#[cfg(test)]
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, Sge, WrOpcode};

pub(super) use accounting::{CqCreditPool, OperationRegistry};
#[cfg(test)]
use batch::test_support::{
    InternalBatchEntry, InternalRelease, commit_internal_entries, release_proven_unaccepted_entries,
};
#[cfg(test)]
use batch::{BatchOwnershipTransfer, PreparedBatchOwnership};
pub(in crate::v2::engine) use batch::{post_io_recv_batch, post_io_send};
pub(in crate::v2::engine) use completion::CqeReject;
#[cfg(test)]
use effects::AfterEngineUnlock;
pub(in crate::v2::engine) use effects::{
    CommittedIoCoreEffects, IoCoreEffects, OperationQuarantineEffect,
};
pub use future::RdmaOperation;
#[cfg(test)]
use future::publish_after_post_guards;
pub(in crate::v2::engine) use reclamation::QpReclaimCapability;
#[cfg(test)]
use state::OperationState;
#[cfg(test)]
use state::{CompletionDisposition, OperationLifecycle};
#[cfg(test)]
pub(in crate::v2::engine) use test_support::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
    operation_future_for_io_lifetime_test, register_operation_waker_for_test,
};

#[cfg(test)]
mod tests;
