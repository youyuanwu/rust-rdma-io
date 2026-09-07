//! Consuming operation effects and detached post-lock publication states.

use std::sync::Arc;

use crate::v2::engine::io::PendingIoEvent;
use crate::v2::engine::registry::{ConnectionToken, OperationToken};
use crate::v2::engine::session::IoEffectsCommitAuthority;

use super::state::OperationState;

#[derive(Default)]
pub(super) struct AfterEngineUnlock {
    events: Vec<PendingIoEvent>,
    operations_to_wake: Vec<Arc<OperationState>>,
}

impl AfterEngineUnlock {
    pub(super) fn from_events(events: Vec<PendingIoEvent>) -> Self {
        Self {
            events,
            operations_to_wake: Vec::new(),
        }
    }

    pub(super) fn push_event(&mut self, event: PendingIoEvent) {
        self.events.push(event);
    }

    pub(super) fn push_operation_wake(&mut self, operation: Arc<OperationState>) {
        self.operations_to_wake.push(operation);
    }

    pub(super) fn extend(&mut self, mut other: Self) {
        self.events.append(&mut other.events);
        self.operations_to_wake
            .append(&mut other.operations_to_wake);
    }

    pub(super) fn publish(self) {
        for event in self.events {
            event.deliver();
        }
        for operation in self.operations_to_wake {
            operation.wake();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum OperationQuarantineEffect {
    Added {
        operation: OperationToken,
        connection: ConnectionToken,
    },
    Cleared {
        operation: OperationToken,
        connection: ConnectionToken,
    },
}

#[derive(Default)]
/// Effects produced after I/O-owned operation and registry mutation completes.
///
/// This full bundle is deliberately not publishable. The session owner must
/// consume it so quarantine and accepted-zero/drain effects are applied before
/// its detached events and operation wakes become available.
pub(in crate::v2::engine) struct IoCoreEffects {
    after_unlock: AfterEngineUnlock,
    quarantine: Vec<OperationQuarantineEffect>,
    drained: Vec<ConnectionToken>,
}

/// Detached I/O publication after the session owner has committed all
/// session-facing effects from the original [`IoCoreEffects`].
///
/// The root terminal path uses this state to preserve CM/connection
/// terminalization before operation notifications. It cannot recover or reuse
/// the original full bundle.
pub(in crate::v2::engine) struct CommittedIoCoreEffects {
    after_unlock: AfterEngineUnlock,
}

/// A detached-only result for a path that cannot produce session effects.
///
/// This is intentionally separate from [`IoCoreEffects`] and
/// [`CommittedIoCoreEffects`]. Its only cross-module use is close-observer
/// notification after admission and lifecycle guards have been released.
pub(in crate::v2::engine) struct DetachedIoCoreEffects {
    after_unlock: AfterEngineUnlock,
}

impl CommittedIoCoreEffects {
    pub(in crate::v2::engine) fn publish(self) {
        self.after_unlock.publish();
    }
}

impl DetachedIoCoreEffects {
    pub(super) fn new(after_unlock: AfterEngineUnlock) -> Self {
        Self { after_unlock }
    }

    pub(in crate::v2::engine) fn publish(self) {
        self.after_unlock.publish();
    }
}

impl IoCoreEffects {
    pub(super) fn extend(&mut self, mut other: Self) {
        self.after_unlock.extend(other.after_unlock);
        self.quarantine.append(&mut other.quarantine);
        self.drained.append(&mut other.drained);
    }

    pub(super) fn push_event(&mut self, event: PendingIoEvent) {
        self.after_unlock.push_event(event);
    }

    pub(super) fn push_operation_wake(&mut self, operation: Arc<OperationState>) {
        self.after_unlock.push_operation_wake(operation);
    }

    pub(super) fn push_quarantine(&mut self, effect: OperationQuarantineEffect) {
        self.quarantine.push(effect);
    }

    pub(super) fn push_drained(&mut self, connection: ConnectionToken) {
        self.drained.push(connection);
    }

    pub(in crate::v2::engine) fn take_quarantine(&mut self) -> Vec<OperationQuarantineEffect> {
        std::mem::take(&mut self.quarantine)
    }

    pub(in crate::v2::engine) fn take_drained(&mut self) -> Vec<ConnectionToken> {
        std::mem::take(&mut self.drained)
    }

    pub(super) fn into_after_unlock(self) -> AfterEngineUnlock {
        assert!(
            self.quarantine.is_empty(),
            "engine must apply operation quarantine effects before publication"
        );
        assert!(
            self.drained.is_empty(),
            "engine must apply accepted-zero effects before publication"
        );
        self.after_unlock
    }

    pub(in crate::v2::engine) fn into_committed(
        self,
        _authority: &IoEffectsCommitAuthority,
    ) -> CommittedIoCoreEffects {
        CommittedIoCoreEffects {
            after_unlock: self.into_after_unlock(),
        }
    }
}
