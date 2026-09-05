//! Owner-neutral contracts used by the explicit engine scheduler.
//!
//! The contracts deliberately report only information needed to schedule
//! another bounded turn. Layer-private identities and lifecycle state stay
//! behind the I/O and session progress owners.

use tokio::time::Instant;

use super::Error;

/// Opaque owner identity used for fair scheduler rotation.
#[allow(
    dead_code,
    reason = "introduced before driver migration in later phases"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OwnerClass {
    Io,
    Session,
    Terminal,
}

#[allow(
    dead_code,
    reason = "introduced before driver migration in later phases"
)]
impl OwnerClass {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Io => 0,
            Self::Session => 1,
            Self::Terminal => 2,
        }
    }
}

/// Result of one finite owner-defined progress turn.
#[allow(
    dead_code,
    reason = "introduced before owner turns migrate in later phases"
)]
pub(super) struct ProgressReport {
    pub(super) units_consumed: usize,
    pub(super) immediate_work: bool,
    pub(super) next_deadline: Option<Instant>,
    pub(super) readiness: ReadinessRegistration,
    pub(super) terminal: ProgressTerminal,
    pub(super) effects: EffectsPublication,
}

/// Whether an owner completed its external-readiness protocol before suspend.
#[allow(
    dead_code,
    reason = "introduced before owner turns migrate in later phases"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadinessRegistration {
    NotRequired,
    RegisteredAndRechecked,
    Incomplete,
}

/// Owner-local terminal information visible to the composition root.
#[allow(
    dead_code,
    reason = "introduced before owner turns migrate in later phases"
)]
pub(super) enum ProgressTerminal {
    Running,
    Ready,
    Failed(Error),
}

/// Proof that user-visible effects from a turn were published after unlock.
#[allow(
    dead_code,
    reason = "introduced before owner turns migrate in later phases"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EffectsPublication {
    Complete,
}

#[allow(
    dead_code,
    reason = "introduced before owner turns migrate in later phases"
)]
impl ProgressReport {
    pub(super) fn idle(next_deadline: Option<Instant>, readiness: ReadinessRegistration) -> Self {
        Self {
            units_consumed: 0,
            immediate_work: false,
            next_deadline,
            readiness,
            terminal: ProgressTerminal::Running,
            effects: EffectsPublication::Complete,
        }
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
        let report = ProgressReport::idle(
            Some(Instant::now()),
            ReadinessRegistration::RegisteredAndRechecked,
        );

        assert_eq!(report.units_consumed, 0);
        assert!(!report.requires_repoll());
        assert!(report.next_deadline.is_some());
        assert!(matches!(report.terminal, ProgressTerminal::Running));
        assert_eq!(report.effects, EffectsPublication::Complete);
    }

    #[test]
    fn incomplete_readiness_requests_repoll() {
        let report = ProgressReport::idle(None, ReadinessRegistration::Incomplete);

        assert!(report.requires_repoll());
    }
}
