//! External-readiness state shared by reactor sources.

/// Whether an owner completed its external-readiness protocol before suspend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadinessRegistration {
    NotRequired,
    RegisteredAndRechecked,
    Incomplete,
}
