//! External-readiness state shared by reactor sources.

#[cfg(test)]
/// Compatibility report used only by focused source tests.
pub(super) struct ProgressReport {
    #[cfg_attr(not(test), allow(dead_code))]
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

#[cfg(test)]
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
