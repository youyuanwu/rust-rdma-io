//! Owner-neutral contracts used by the explicit engine scheduler.
//!
//! The contracts deliberately report only information needed to schedule
//! another bounded turn. Layer-private identities and lifecycle state stay
//! behind the I/O and session progress owners.

/// Opaque owner identity used for fair scheduler rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OwnerClass {
    Io,
    Session,
}

impl OwnerClass {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Io => 0,
            Self::Session => 1,
        }
    }
}

/// Result of one finite owner-defined progress turn.
pub(super) struct ProgressReport {
    pub(super) units_consumed: usize,
    pub(super) immediate_work: bool,
    pub(super) readiness: ReadinessRegistration,
}

/// Whether an owner completed its external-readiness protocol before suspend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadinessRegistration {
    NotRequired,
    RegisteredAndRechecked,
    Incomplete,
}

impl ProgressReport {
    pub(super) fn running(
        units_consumed: usize,
        immediate_work: bool,
        readiness: ReadinessRegistration,
    ) -> Self {
        Self {
            units_consumed,
            immediate_work,
            readiness,
        }
    }

    #[cfg(test)]
    pub(super) fn idle(readiness: ReadinessRegistration) -> Self {
        Self::running(0, false, readiness)
    }

    pub(super) fn requires_repoll(&self) -> bool {
        self.immediate_work || self.readiness == ReadinessRegistration::Incomplete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_registered_report_does_not_request_repoll() {
        let report = ProgressReport::idle(ReadinessRegistration::RegisteredAndRechecked);

        assert_eq!(report.units_consumed, 0);
        assert!(!report.requires_repoll());
        assert_eq!(
            report.readiness,
            ReadinessRegistration::RegisteredAndRechecked
        );
    }

    #[test]
    fn incomplete_readiness_requests_repoll() {
        let report = ProgressReport::idle(ReadinessRegistration::Incomplete);

        assert!(report.requires_repoll());
    }
}
