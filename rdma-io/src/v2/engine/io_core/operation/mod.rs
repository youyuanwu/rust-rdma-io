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

pub(super) use accounting::{CqCreditPool, OperationRegistry};
pub(in crate::v2::engine) use batch::{post_io_recv_batch_into, post_io_send_into};
pub(in crate::v2::engine) use completion::CqeReject;
pub(in crate::v2::engine) use effects::{
    AfterEngineUnlock, IoCoreEffects, OperationQuarantineEffect,
};
pub use future::RdmaOperation;
pub(in crate::v2::engine) use state::OperationObserver;
#[cfg(test)]
pub(in crate::v2::engine) use test_support::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
    operation_future_for_io_lifetime_test, register_operation_waker_for_test,
};

#[cfg(test)]
mod tests;
