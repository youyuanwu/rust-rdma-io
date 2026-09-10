//! Reactor-owned generational connection lifecycle and exact secondary indexes.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::super::io_core::{ConnectionIoState, EstablishedIoConnection};
use super::super::registry::{
    ConnectionToken, ListenerToken, LiveIoConnectionProof, Lookup, OperationToken, PagedRegistry,
};
use super::cm::{InboundRoute, InboundState, OutboundRequest, OutboundRoute, OutboundState};
use super::connection::{
    ConnectionCmRoute, ConnectionDiagnosticsGauge, ConnectionPoster, ConnectionReservation,
    ConnectionState, ConnectionStateCountSnapshot,
};
use super::{DeadlineKind, DeadlineRequest};
use crate::v2::error::{Error, Result};

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum ConnectionPhase {
    Outbound,
    Inbound,
    Active,
    Draining,
    Retiring,
    Quarantined,
    Retired,
}

#[derive(Clone)]
pub(in crate::v2::engine) struct ConnectionSnapshot {
    pub(in crate::v2::engine) qp_num: u32,
}

enum EstablishedRoute {
    Outbound(OutboundRoute),
    Inbound(InboundRoute),
    None,
}

struct LiveConnectionEntry {
    route: EstablishedRoute,
    connection: ConnectionState,
}

#[allow(
    clippy::large_enum_variant,
    reason = "the generational slot owns each route and connection bundle in place"
)]
enum OutboundConnectionEntry {
    Establishing(OutboundRoute),
    Registered {
        route: OutboundRoute,
        connection: ConnectionState,
    },
}

#[allow(
    clippy::large_enum_variant,
    reason = "the generational slot owns each route and connection bundle in place"
)]
enum InboundConnectionEntry {
    Establishing(InboundRoute),
    Registered {
        route: InboundRoute,
        connection: ConnectionState,
    },
}

enum QuarantinedLifecycle {
    OutboundSetup {
        route: OutboundRoute,
        _poster: ConnectionPoster,
        reservation: ConnectionReservation,
    },
    InboundSetup {
        route: InboundRoute,
        _poster: ConnectionPoster,
        reservation: ConnectionReservation,
    },
    Outbound(OutboundConnectionEntry),
    Inbound(InboundConnectionEntry),
    Active(LiveConnectionEntry),
    Draining(LiveConnectionEntry),
    Retiring {
        connection: LiveConnectionEntry,
        started: bool,
    },
}

struct QuarantinedConnectionEntry {
    lifecycle: QuarantinedLifecycle,
    bundle: bool,
}

/// The sole owning lifecycle value for one generational connection.
///
/// The entry is deliberately non-cloneable. Direction-specific CM state,
/// admission, the QP/resource owner, accepted-work ledger, close state,
/// retirement state, and quarantine all move together through this enum.
enum ConnectionEntry {
    Outbound(OutboundConnectionEntry),
    Inbound(InboundConnectionEntry),
    Active(LiveConnectionEntry),
    Draining(LiveConnectionEntry),
    Retiring {
        connection: LiveConnectionEntry,
        started: bool,
    },
    Quarantined(QuarantinedConnectionEntry),
    Retired(LiveConnectionEntry),
    Transitioning,
}

impl ConnectionEntry {
    #[cfg(test)]
    fn phase(&self) -> ConnectionPhase {
        match self {
            Self::Outbound(_) => ConnectionPhase::Outbound,
            Self::Inbound(_) => ConnectionPhase::Inbound,
            Self::Active(_) => ConnectionPhase::Active,
            Self::Draining(_) => ConnectionPhase::Draining,
            Self::Retiring { .. } => ConnectionPhase::Retiring,
            Self::Quarantined(_) => ConnectionPhase::Quarantined,
            Self::Retired(_) => ConnectionPhase::Retired,
            Self::Transitioning => {
                unreachable!("connection entries are never observed while transitioning")
            }
        }
    }

    fn connection(&self) -> Option<&ConnectionState> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                Some(connection)
            }
            Self::Active(connection) | Self::Draining(connection) | Self::Retired(connection) => {
                Some(&connection.connection)
            }
            Self::Retiring { connection, .. } => Some(&connection.connection),
            Self::Quarantined(entry) => entry.lifecycle.connection(),
            Self::Outbound(OutboundConnectionEntry::Establishing(_))
            | Self::Inbound(InboundConnectionEntry::Establishing(_))
            | Self::Transitioning => None,
        }
    }

    fn connection_mut(&mut self) -> Option<&mut ConnectionState> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                Some(connection)
            }
            Self::Active(connection) | Self::Draining(connection) | Self::Retired(connection) => {
                Some(&mut connection.connection)
            }
            Self::Retiring { connection, .. } => Some(&mut connection.connection),
            Self::Quarantined(entry) => entry.lifecycle.connection_mut(),
            Self::Outbound(OutboundConnectionEntry::Establishing(_))
            | Self::Inbound(InboundConnectionEntry::Establishing(_))
            | Self::Transitioning => None,
        }
    }

    fn route(&self) -> Option<&EstablishedRoute> {
        match self {
            Self::Active(connection) | Self::Draining(connection) | Self::Retired(connection) => {
                Some(&connection.route)
            }
            Self::Retiring { connection, .. } => Some(&connection.route),
            Self::Quarantined(entry) => entry.lifecycle.route(),
            Self::Outbound(_) | Self::Inbound(_) | Self::Transitioning => None,
        }
    }

    fn route_mut(&mut self) -> Option<&mut EstablishedRoute> {
        match self {
            Self::Active(connection) | Self::Draining(connection) | Self::Retired(connection) => {
                Some(&mut connection.route)
            }
            Self::Retiring { connection, .. } => Some(&mut connection.route),
            Self::Quarantined(entry) => entry.lifecycle.route_mut(),
            Self::Outbound(_) | Self::Inbound(_) | Self::Transitioning => None,
        }
    }

    #[cfg(test)]
    fn reservation(&self) -> Option<&ConnectionReservation> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                connection.admission.as_ref()
            }
            Self::Active(connection) | Self::Draining(connection) | Self::Retired(connection) => {
                connection.connection.admission.as_ref()
            }
            Self::Retiring { connection, .. } => connection.connection.admission.as_ref(),
            Self::Quarantined(entry) => entry.lifecycle.reservation(),
            Self::Outbound(OutboundConnectionEntry::Establishing(route)) => match &route.state {
                OutboundState::AwaitAddr { reservation, .. }
                | OutboundState::AwaitRoute { reservation, .. } => Some(reservation),
                _ => None,
            },
            Self::Inbound(InboundConnectionEntry::Establishing(route)) => match &route.state {
                InboundState::PendingSelection { reservation, .. } => Some(reservation),
                _ => None,
            },
            Self::Transitioning => None,
        }
    }

    fn reservation_mut(&mut self) -> Option<&mut ConnectionReservation> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                connection.admission.as_mut()
            }
            Self::Active(connection) | Self::Draining(connection) | Self::Retired(connection) => {
                connection.connection.admission.as_mut()
            }
            Self::Retiring { connection, .. } => connection.connection.admission.as_mut(),
            Self::Quarantined(entry) => entry.lifecycle.reservation_mut(),
            Self::Outbound(OutboundConnectionEntry::Establishing(route)) => {
                match &mut route.state {
                    OutboundState::AwaitAddr { reservation, .. }
                    | OutboundState::AwaitRoute { reservation, .. } => Some(reservation),
                    _ => None,
                }
            }
            Self::Inbound(InboundConnectionEntry::Establishing(route)) => match &mut route.state {
                InboundState::PendingSelection { reservation, .. } => Some(reservation),
                _ => None,
            },
            Self::Transitioning => None,
        }
    }

    fn outbound_route(&self) -> Option<&OutboundRoute> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Establishing(route))
            | Self::Outbound(OutboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Quarantined(entry) => entry.lifecycle.outbound_route(),
            _ => match self.route()? {
                EstablishedRoute::Outbound(route) => Some(route),
                EstablishedRoute::Inbound(_) | EstablishedRoute::None => None,
            },
        }
    }

    fn outbound_route_mut(&mut self) -> Option<&mut OutboundRoute> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Establishing(route))
            | Self::Outbound(OutboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Quarantined(entry) => entry.lifecycle.outbound_route_mut(),
            _ => match self.route_mut()? {
                EstablishedRoute::Outbound(route) => Some(route),
                EstablishedRoute::Inbound(_) | EstablishedRoute::None => None,
            },
        }
    }

    fn inbound_route(&self) -> Option<&InboundRoute> {
        match self {
            Self::Inbound(InboundConnectionEntry::Establishing(route))
            | Self::Inbound(InboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Quarantined(entry) => entry.lifecycle.inbound_route(),
            _ => match self.route()? {
                EstablishedRoute::Inbound(route) => Some(route),
                EstablishedRoute::Outbound(_) | EstablishedRoute::None => None,
            },
        }
    }

    fn inbound_route_mut(&mut self) -> Option<&mut InboundRoute> {
        match self {
            Self::Inbound(InboundConnectionEntry::Establishing(route))
            | Self::Inbound(InboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Quarantined(entry) => entry.lifecycle.inbound_route_mut(),
            _ => match self.route_mut()? {
                EstablishedRoute::Inbound(route) => Some(route),
                EstablishedRoute::Outbound(_) | EstablishedRoute::None => None,
            },
        }
    }
}

impl QuarantinedLifecycle {
    fn outbound_route(&self) -> Option<&OutboundRoute> {
        match self {
            Self::OutboundSetup { route, .. } => Some(route),
            Self::Outbound(OutboundConnectionEntry::Establishing(route))
            | Self::Outbound(OutboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Active(connection)
            | Self::Draining(connection)
            | Self::Retiring { connection, .. } => match &connection.route {
                EstablishedRoute::Outbound(route) => Some(route),
                EstablishedRoute::Inbound(_) | EstablishedRoute::None => None,
            },
            Self::InboundSetup { .. } | Self::Inbound(_) => None,
        }
    }

    fn outbound_route_mut(&mut self) -> Option<&mut OutboundRoute> {
        match self {
            Self::OutboundSetup { route, .. } => Some(route),
            Self::Outbound(OutboundConnectionEntry::Establishing(route))
            | Self::Outbound(OutboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Active(connection)
            | Self::Draining(connection)
            | Self::Retiring { connection, .. } => match &mut connection.route {
                EstablishedRoute::Outbound(route) => Some(route),
                EstablishedRoute::Inbound(_) | EstablishedRoute::None => None,
            },
            Self::InboundSetup { .. } | Self::Inbound(_) => None,
        }
    }

    fn inbound_route(&self) -> Option<&InboundRoute> {
        match self {
            Self::InboundSetup { route, .. } => Some(route),
            Self::Inbound(InboundConnectionEntry::Establishing(route))
            | Self::Inbound(InboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Active(connection)
            | Self::Draining(connection)
            | Self::Retiring { connection, .. } => match &connection.route {
                EstablishedRoute::Inbound(route) => Some(route),
                EstablishedRoute::Outbound(_) | EstablishedRoute::None => None,
            },
            Self::OutboundSetup { .. } | Self::Outbound(_) => None,
        }
    }

    fn inbound_route_mut(&mut self) -> Option<&mut InboundRoute> {
        match self {
            Self::InboundSetup { route, .. } => Some(route),
            Self::Inbound(InboundConnectionEntry::Establishing(route))
            | Self::Inbound(InboundConnectionEntry::Registered { route, .. }) => Some(route),
            Self::Active(connection)
            | Self::Draining(connection)
            | Self::Retiring { connection, .. } => match &mut connection.route {
                EstablishedRoute::Inbound(route) => Some(route),
                EstablishedRoute::Outbound(_) | EstablishedRoute::None => None,
            },
            Self::OutboundSetup { .. } | Self::Outbound(_) => None,
        }
    }

    fn connection(&self) -> Option<&ConnectionState> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                Some(connection)
            }
            Self::Active(connection) | Self::Draining(connection) => Some(&connection.connection),
            Self::Retiring { connection, .. } => Some(&connection.connection),
            Self::OutboundSetup { .. } | Self::InboundSetup { .. } => None,
            Self::Outbound(OutboundConnectionEntry::Establishing(_))
            | Self::Inbound(InboundConnectionEntry::Establishing(_)) => None,
        }
    }

    fn connection_mut(&mut self) -> Option<&mut ConnectionState> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                Some(connection)
            }
            Self::Active(connection) | Self::Draining(connection) => {
                Some(&mut connection.connection)
            }
            Self::Retiring { connection, .. } => Some(&mut connection.connection),
            Self::OutboundSetup { .. } | Self::InboundSetup { .. } => None,
            Self::Outbound(OutboundConnectionEntry::Establishing(_))
            | Self::Inbound(InboundConnectionEntry::Establishing(_)) => None,
        }
    }

    fn route(&self) -> Option<&EstablishedRoute> {
        match self {
            Self::Active(connection) | Self::Draining(connection) => Some(&connection.route),
            Self::Retiring { connection, .. } => Some(&connection.route),
            Self::OutboundSetup { .. }
            | Self::InboundSetup { .. }
            | Self::Outbound(_)
            | Self::Inbound(_) => None,
        }
    }

    fn route_mut(&mut self) -> Option<&mut EstablishedRoute> {
        match self {
            Self::Active(connection) | Self::Draining(connection) => Some(&mut connection.route),
            Self::Retiring { connection, .. } => Some(&mut connection.route),
            Self::OutboundSetup { .. }
            | Self::InboundSetup { .. }
            | Self::Outbound(_)
            | Self::Inbound(_) => None,
        }
    }

    fn close_started(&self) -> bool {
        matches!(self, Self::Draining(_) | Self::Retiring { .. })
    }

    #[cfg(test)]
    fn reservation(&self) -> Option<&ConnectionReservation> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                connection.admission.as_ref()
            }
            Self::Active(connection) | Self::Draining(connection) => {
                connection.connection.admission.as_ref()
            }
            Self::Retiring { connection, .. } => connection.connection.admission.as_ref(),
            Self::OutboundSetup { reservation, .. } | Self::InboundSetup { reservation, .. } => {
                Some(reservation)
            }
            Self::Outbound(OutboundConnectionEntry::Establishing(route)) => match &route.state {
                OutboundState::AwaitAddr { reservation, .. }
                | OutboundState::AwaitRoute { reservation, .. } => Some(reservation),
                _ => None,
            },
            Self::Inbound(InboundConnectionEntry::Establishing(route)) => match &route.state {
                InboundState::PendingSelection { reservation, .. } => Some(reservation),
                _ => None,
            },
        }
    }

    fn reservation_mut(&mut self) -> Option<&mut ConnectionReservation> {
        match self {
            Self::Outbound(OutboundConnectionEntry::Registered { connection, .. })
            | Self::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                connection.admission.as_mut()
            }
            Self::Active(connection) | Self::Draining(connection) => {
                connection.connection.admission.as_mut()
            }
            Self::Retiring { connection, .. } => connection.connection.admission.as_mut(),
            Self::OutboundSetup { reservation, .. } | Self::InboundSetup { reservation, .. } => {
                Some(reservation)
            }
            Self::Outbound(OutboundConnectionEntry::Establishing(route)) => {
                match &mut route.state {
                    OutboundState::AwaitAddr { reservation, .. }
                    | OutboundState::AwaitRoute { reservation, .. } => Some(reservation),
                    _ => None,
                }
            }
            Self::Inbound(InboundConnectionEntry::Establishing(route)) => match &mut route.state {
                InboundState::PendingSelection { reservation, .. } => Some(reservation),
                _ => None,
            },
        }
    }
}

pub(in crate::v2::engine) struct ConnectionRegistry {
    slots: PagedRegistry<ConnectionToken, ConnectionEntry>,
    qp_index: HashMap<u32, ConnectionToken>,
    capacity: usize,
    admission: Arc<Semaphore>,
    deadline_requests: VecDeque<DeadlineRequest>,
    pending_outbound: VecDeque<(Arc<OutboundRequest>, ConnectionReservation)>,
    cancellations: VecDeque<Arc<OutboundRequest>>,
    retirements: VecDeque<ConnectionToken>,
    outbound_setup_active: bool,
    route_count: usize,
    diagnostics: Arc<ConnectionDiagnosticsGauge>,
}

impl ConnectionRegistry {
    pub(in crate::v2::engine) fn establish_qp_destruction_proof(
        &mut self,
        token: ConnectionToken,
    ) -> Result<super::QpDestructionProof> {
        let status = self
            .with_connection_mut(token, |connection| connection.destroy_qp_for_session())
            .ok_or(Error::TransportClosed)??;
        match status {
            super::connection::QpDestroyStatus::DestroyedNow => Ok(super::QpDestructionProof {
                connection: token,
                qp_num: self
                    .with_connection(token, |connection| connection.qp_num())
                    .ok_or(Error::TransportClosed)?,
                _evidence: (),
            }),
            super::connection::QpDestroyStatus::AlreadyDestroyed => Err(Error::InvalidConfig(
                "QP destruction proof was already minted and cannot be replayed".into(),
            )),
        }
    }

    pub(in crate::v2::engine) fn ensure_qp_destroyed(
        &mut self,
        token: ConnectionToken,
    ) -> Result<()> {
        match self
            .with_connection_mut(token, |connection| connection.destroy_qp_for_session())
            .ok_or(Error::TransportClosed)??
        {
            super::connection::QpDestroyStatus::DestroyedNow
            | super::connection::QpDestroyStatus::AlreadyDestroyed => Ok(()),
        }
    }

    pub(in crate::v2::engine) fn transition_connection_to_error(
        &mut self,
        token: ConnectionToken,
    ) -> Result<bool> {
        self.with_connection_mut(token, |connection| connection.transition_to_error_once())
            .ok_or(Error::TransportClosed)?
    }

    pub(in crate::v2::engine) fn finalize_connection_engine(
        &mut self,
        token: ConnectionToken,
        outcome: &super::super::lifecycle::MemoizedTerminalResult,
    ) -> Option<super::super::io::PendingIoEvent> {
        self.with_connection_mut(token, |connection| {
            connection.close_state().record_engine_terminal(outcome);
            connection.finalize_engine(outcome)
        })
        .flatten()
    }

    pub(in crate::v2::engine) fn finalize_quarantined_connection_engine(
        &mut self,
        token: ConnectionToken,
        outcome: &super::super::lifecycle::MemoizedTerminalResult,
    ) -> Option<super::super::io::PendingIoEvent> {
        self.with_connection_mut(token, |connection| {
            connection.close_state().record_engine_terminal(outcome);
            connection.finalize_engine_without_provider(outcome)
        })
        .flatten()
    }

    pub(in crate::v2::engine) fn destroy_connection_resources(
        &mut self,
        token: ConnectionToken,
        outstanding_operations: usize,
    ) -> Result<Option<super::connection::SharedCmId>> {
        self.with_connection_mut(token, |connection| {
            connection.destroy_connection_resources(outstanding_operations)
        })
        .ok_or(Error::TransportClosed)?
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
        Self::new_with_admission(capacity, Arc::new(Semaphore::new(capacity)))
    }

    pub(in crate::v2::engine) fn new_with_admission(
        capacity: usize,
        admission: Arc<Semaphore>,
    ) -> Result<Self> {
        Ok(Self {
            slots: PagedRegistry::new(capacity)?,
            qp_index: HashMap::new(),
            capacity,
            admission,
            deadline_requests: VecDeque::new(),
            pending_outbound: VecDeque::new(),
            cancellations: VecDeque::new(),
            retirements: VecDeque::new(),
            outbound_setup_active: false,
            route_count: 0,
            diagnostics: Arc::new(ConnectionDiagnosticsGauge::default()),
        })
    }

    pub(in crate::v2::engine) fn enqueue_outbound(
        &mut self,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
    ) {
        self.pending_outbound.push_back((request, reservation));
    }

    pub(in crate::v2::engine) fn pop_outbound(
        &mut self,
    ) -> Option<(Arc<OutboundRequest>, ConnectionReservation)> {
        self.pending_outbound.pop_front()
    }

    pub(in crate::v2::engine) fn push_front_outbound(
        &mut self,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
    ) {
        self.pending_outbound.push_front((request, reservation));
    }

    pub(in crate::v2::engine) fn pending_outbound_count(&self) -> usize {
        self.pending_outbound.len()
    }

    pub(in crate::v2::engine) fn drain_pending_outbound(
        &mut self,
    ) -> impl Iterator<Item = (Arc<OutboundRequest>, ConnectionReservation)> + '_ {
        self.pending_outbound.drain(..)
    }

    pub(in crate::v2::engine) fn enqueue_cancellation(&mut self, request: Arc<OutboundRequest>) {
        if request.try_enqueue_cancellation() {
            self.cancellations.push_back(request);
        }
    }

    pub(in crate::v2::engine) fn pop_cancellation(&mut self) -> Option<Arc<OutboundRequest>> {
        self.cancellations.pop_front()
    }

    pub(in crate::v2::engine) fn cancellation_count(&self) -> usize {
        self.cancellations.len()
    }

    pub(super) fn enqueue_retirement(&mut self, token: ConnectionToken) {
        self.retirements.push_back(token);
    }

    pub(in crate::v2::engine) fn pop_retirement(&mut self) -> Option<ConnectionToken> {
        self.retirements.pop_front()
    }

    pub(in crate::v2::engine) fn retirement_count(&self) -> usize {
        self.retirements.len()
    }

    pub(in crate::v2::engine) fn try_begin_outbound_setup(&mut self) -> bool {
        if self.outbound_setup_active {
            false
        } else {
            self.outbound_setup_active = true;
            true
        }
    }

    pub(in crate::v2::engine) fn finish_outbound_setup(&mut self) {
        self.outbound_setup_active = false;
    }

    pub(in crate::v2::engine) fn schedule_deadline(
        &mut self,
        kind: DeadlineKind,
        token: u64,
        after: std::time::Duration,
    ) {
        let now = tokio::time::Instant::now();
        let at = now.checked_add(after).unwrap_or(now);
        self.deadline_requests
            .push_back(DeadlineRequest { at, kind, token });
    }

    pub(in crate::v2::engine) fn take_deadline_requests(
        &mut self,
        budget: usize,
    ) -> Vec<DeadlineRequest> {
        let count = self.deadline_requests.len().min(budget);
        self.deadline_requests.drain(..count).collect()
    }

    pub(in crate::v2::engine) fn deadline_request_count(&self) -> usize {
        self.deadline_requests.len()
    }

    pub(in crate::v2::engine) fn try_reserve(&self) -> Option<ConnectionReservation> {
        Arc::clone(&self.admission)
            .try_acquire_owned()
            .ok()
            .map(|permit| {
                ConnectionReservation::new_with_diagnostics(
                    permit,
                    Some(Arc::clone(&self.diagnostics)),
                )
            })
    }

    pub(in crate::v2::engine) fn admission_snapshot(&self) -> ConnectionStateCountSnapshot {
        let mut snapshot = self.diagnostics.snapshot();
        snapshot.live = self
            .capacity
            .saturating_sub(self.admission.available_permits());
        snapshot
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn admission_snapshot_excluding_retained(
        &self,
    ) -> ConnectionStateCountSnapshot {
        self.diagnostics.snapshot_excluding_retained()
    }

    pub(super) fn register_outbound(
        &mut self,
        make: impl FnOnce(ConnectionToken) -> OutboundRoute,
    ) -> Result<ConnectionToken> {
        let token = self.slots.allocate_owned(|token| {
            ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(make(token)))
        })?;
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::reservation_mut)
            .into_iter()
            .for_each(|reservation| reservation.mark_indexed(Arc::clone(&self.diagnostics)));
        self.route_count += 1;
        Ok(token)
    }

    pub(super) fn register_inbound(
        &mut self,
        make: impl FnOnce(ConnectionToken) -> InboundRoute,
    ) -> Result<ConnectionToken> {
        let token = self.slots.allocate_owned(|token| {
            ConnectionEntry::Inbound(InboundConnectionEntry::Establishing(make(token)))
        })?;
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::reservation_mut)
            .into_iter()
            .for_each(|reservation| reservation.mark_indexed(Arc::clone(&self.diagnostics)));
        self.route_count += 1;
        Ok(token)
    }

    #[allow(
        clippy::result_large_err,
        reason = "registration failure returns complete retained ownership for rollback"
    )]
    pub(in crate::v2::engine) fn register(
        &mut self,
        qp_num: u32,
        make: impl FnOnce(ConnectionToken) -> ConnectionState,
    ) -> std::result::Result<(ConnectionToken, ConnectionSnapshot), ConnectionRegistrationFailure>
    {
        self.validate_qp(qp_num)?;
        let mut snapshot = None;
        let token = self
            .slots
            .allocate_owned(|token| {
                let connection = make(token);
                snapshot = Some(Self::snapshot(&connection));
                ConnectionEntry::Active(LiveConnectionEntry {
                    route: EstablishedRoute::None,
                    connection,
                })
            })
            .map_err(|error| ConnectionRegistrationFailure {
                error,
                retained: None,
            })?;
        self.qp_index.insert(qp_num, token);
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::reservation_mut)
            .into_iter()
            .for_each(|reservation| reservation.mark_indexed(Arc::clone(&self.diagnostics)));
        Ok((
            token,
            snapshot.expect("connection registration factory runs exactly once"),
        ))
    }

    #[allow(
        clippy::result_large_err,
        reason = "attachment failure returns complete retained ownership for rollback"
    )]
    pub(in crate::v2::engine) fn attach_registered(
        &mut self,
        token: ConnectionToken,
        qp_num: u32,
        connection: ConnectionState,
    ) -> std::result::Result<ConnectionSnapshot, ConnectionRegistrationFailure> {
        if let Err(mut failure) = self.validate_qp(qp_num) {
            failure.retained = Some((token, connection));
            return Err(failure);
        }
        let Some(entry) = self.slots.get_mut(token) else {
            return Err(ConnectionRegistrationFailure {
                error: Error::InvalidConfig(
                    "connection route generation disappeared before QP registration".into(),
                ),
                retained: Some((token, connection)),
            });
        };
        let current = std::mem::replace(entry, ConnectionEntry::Transitioning);
        let next = match current {
            ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(route)) => {
                ConnectionEntry::Outbound(OutboundConnectionEntry::Registered { route, connection })
            }
            ConnectionEntry::Inbound(InboundConnectionEntry::Establishing(route)) => {
                ConnectionEntry::Inbound(InboundConnectionEntry::Registered { route, connection })
            }
            other => {
                *entry = other;
                return Err(ConnectionRegistrationFailure {
                    error: Error::InvalidConfig(
                        "connection route registered more than one QP owner".into(),
                    ),
                    retained: Some((token, connection)),
                });
            }
        };
        *entry = next;
        self.qp_index.insert(qp_num, token);
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::reservation_mut)
            .into_iter()
            .for_each(|reservation| reservation.mark_indexed(Arc::clone(&self.diagnostics)));
        Ok(self
            .lookup(token)
            .occupied()
            .expect("attached connection is immediately visible"))
    }

    #[allow(
        clippy::result_large_err,
        reason = "the shared failure type preserves rollback ownership at every validation exit"
    )]
    fn validate_qp(&self, qp_num: u32) -> std::result::Result<(), ConnectionRegistrationFailure> {
        if qp_num == 0 {
            return Err(ConnectionRegistrationFailure {
                error: Error::InvalidConfig("provider returned zero qp_num".into()),
                retained: None,
            });
        }
        if self.qp_index.contains_key(&qp_num) {
            return Err(ConnectionRegistrationFailure {
                error: Error::InvalidConfig(format!("qp_num {qp_num} is already registered")),
                retained: None,
            });
        }
        Ok(())
    }

    pub(super) fn validate_qp_registration(&self, qp_num: u32) -> Result<()> {
        self.validate_qp(qp_num).map_err(|failure| failure.error)
    }

    fn snapshot(connection: &ConnectionState) -> ConnectionSnapshot {
        ConnectionSnapshot {
            qp_num: connection.qp_num(),
        }
    }

    pub(in crate::v2::engine) fn lookup(
        &self,
        token: ConnectionToken,
    ) -> Lookup<ConnectionSnapshot> {
        match self.slots.lookup_ref(token) {
            Lookup::Occupied(entry) => entry.connection().map_or(Lookup::Unknown, |connection| {
                Lookup::Occupied(Self::snapshot(connection))
            }),
            Lookup::Duplicate => Lookup::Duplicate,
            Lookup::Stale => Lookup::Stale,
            Lookup::Unknown => Lookup::Unknown,
            Lookup::Retired => Lookup::Retired,
        }
    }

    pub(in crate::v2::engine) fn with_connection<T>(
        &self,
        token: ConnectionToken,
        inspect: impl FnOnce(&ConnectionState) -> T,
    ) -> Option<T> {
        self.slots
            .lookup_ref(token)
            .occupied()
            .and_then(ConnectionEntry::connection)
            .map(inspect)
    }

    pub(in crate::v2::engine) fn with_connection_mut<T>(
        &mut self,
        token: ConnectionToken,
        mutate: impl FnOnce(&mut ConnectionState) -> T,
    ) -> Option<T> {
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::connection_mut)
            .map(mutate)
    }

    pub(in crate::v2::engine) fn with_connection_io_mut<T>(
        &mut self,
        token: ConnectionToken,
        mutate: impl FnOnce(
            &Arc<EstablishedIoConnection>,
            &mut ConnectionIoState,
            &super::connection::ConnectionPoster,
        ) -> T,
    ) -> Option<T> {
        self.with_connection_mut(token, |connection| {
            let (io, io_ledger, poster) = connection.io_parts_mut();
            let io = Arc::clone(io);
            mutate(&io, io_ledger, poster)
        })
    }

    pub(in crate::v2::engine) fn io_is_open(&self, token: ConnectionToken) -> bool {
        matches!(
            self.slots.lookup_ref(token),
            Lookup::Occupied(ConnectionEntry::Active(connection))
                if connection.connection.io_is_open()
        )
    }

    pub(in crate::v2::engine) fn close_started(&self, token: ConnectionToken) -> bool {
        match self.slots.lookup_ref(token) {
            Lookup::Occupied(ConnectionEntry::Draining(_))
            | Lookup::Occupied(ConnectionEntry::Retiring { .. }) => true,
            Lookup::Occupied(ConnectionEntry::Quarantined(entry)) => {
                entry.lifecycle.close_started()
            }
            _ => false,
        }
    }

    pub(in crate::v2::engine) fn is_quarantined(&self, token: ConnectionToken) -> bool {
        matches!(
            self.slots.lookup_ref(token),
            Lookup::Occupied(ConnectionEntry::Quarantined(_))
        )
    }

    pub(in crate::v2::engine) fn accepted_count(&self, token: ConnectionToken) -> usize {
        self.with_connection(token, |connection| connection.io_ledger.accepted_count())
            .unwrap_or(0)
    }

    pub(in crate::v2::engine) fn accepted_tokens_bounded(
        &self,
        token: ConnectionToken,
        limit: usize,
    ) -> Vec<OperationToken> {
        self.with_connection(token, |connection| {
            connection.io_ledger.accepted_tokens_bounded(limit)
        })
        .unwrap_or_default()
    }

    pub(in crate::v2::engine) fn has_completion_work(&self, token: ConnectionToken) -> bool {
        self.with_connection(token, |connection| {
            connection.io_ledger.has_completion_work()
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn close_scan(
        &self,
        token: ConnectionToken,
    ) -> Option<(usize, bool)> {
        self.with_connection(token, |connection| {
            (
                connection.close_operation_scan_slot(),
                connection.close_operation_scan_complete(),
            )
        })
    }

    pub(in crate::v2::engine) fn update_close_scan(
        &mut self,
        token: ConnectionToken,
        next: usize,
        complete: bool,
    ) -> bool {
        self.with_connection_mut(token, |connection| {
            connection.update_close_operation_scan(next, complete)
        })
        .is_some()
    }

    pub(in crate::v2::engine) fn quarantine_scan(
        &self,
        token: ConnectionToken,
    ) -> Option<(usize, bool)> {
        self.with_connection(token, |connection| {
            (
                connection.quarantine_operation_scan_slot(),
                connection.quarantine_operation_scan_complete(),
            )
        })
    }

    pub(in crate::v2::engine) fn update_quarantine_scan(
        &mut self,
        token: ConnectionToken,
        next: usize,
        complete: bool,
    ) -> bool {
        self.with_connection_mut(token, |connection| {
            connection.update_quarantine_operation_scan(next, complete)
        })
        .is_some()
    }

    pub(in crate::v2::engine) fn take_qp_reclamation_proof(
        &mut self,
        token: ConnectionToken,
    ) -> Option<super::QpDestructionProof> {
        self.with_connection_mut(token, ConnectionState::take_qp_reclamation_proof)
            .flatten()
    }

    pub(in crate::v2::engine) fn store_qp_reclamation_proof(
        &mut self,
        token: ConnectionToken,
        proof: super::QpDestructionProof,
    ) -> bool {
        self.with_connection_mut(token, |connection| {
            connection.store_qp_reclamation_proof(proof)
        })
        .is_some()
    }

    pub(in crate::v2::engine) fn mark_drained_once(&mut self, token: ConnectionToken) -> bool {
        self.with_connection_mut(token, ConnectionState::mark_drained_once)
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn publish_destroy_quarantine_for_test(
        &mut self,
        token: ConnectionToken,
        error: &Error,
    ) -> bool {
        self.with_connection_mut(token, |connection| {
            connection.publish_destroy_quarantine(error, || {}).0
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn wake_close_into(
        &self,
        token: ConnectionToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let _ = self.with_connection(token, |connection| connection.wake_close_into(actions));
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn wake_close(&self, token: ConnectionToken) {
        let _ = self.with_connection(token, ConnectionState::wake_close);
    }

    pub(super) fn lookup_outbound(
        &self,
        token: ConnectionToken,
    ) -> Lookup<ConnectionRouteIdentity> {
        self.lookup_route(token, ConnectionRouteDirection::Outbound)
    }

    pub(super) fn lookup_inbound(&self, token: ConnectionToken) -> Lookup<ConnectionRouteIdentity> {
        self.lookup_route(token, ConnectionRouteDirection::Inbound)
    }

    fn lookup_route(
        &self,
        token: ConnectionToken,
        direction: ConnectionRouteDirection,
    ) -> Lookup<ConnectionRouteIdentity> {
        match self.slots.lookup_ref(token) {
            Lookup::Occupied(entry) => {
                let route = match direction {
                    ConnectionRouteDirection::Outbound => {
                        entry.outbound_route().map(|route| ConnectionRouteIdentity {
                            token,
                            raw_id: route.raw_id,
                            context_key: route.context_key,
                            direction,
                        })
                    }
                    ConnectionRouteDirection::Inbound => {
                        entry.inbound_route().map(|route| ConnectionRouteIdentity {
                            token,
                            raw_id: route.raw_id,
                            context_key: route.context_key,
                            direction,
                        })
                    }
                };
                route.map_or(Lookup::Unknown, Lookup::Occupied)
            }
            Lookup::Duplicate => Lookup::Duplicate,
            Lookup::Stale => Lookup::Stale,
            Lookup::Unknown => Lookup::Unknown,
            Lookup::Retired => Lookup::Retired,
        }
    }

    pub(super) fn with_outbound_route<T>(
        &self,
        token: ConnectionToken,
        inspect: impl FnOnce(&OutboundRoute) -> T,
    ) -> Option<T> {
        self.slots
            .lookup_ref(token)
            .occupied()
            .and_then(ConnectionEntry::outbound_route)
            .map(inspect)
    }

    pub(super) fn with_outbound_route_mut<T>(
        &mut self,
        token: ConnectionToken,
        mutate: impl FnOnce(&mut OutboundRoute) -> T,
    ) -> Option<T> {
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::outbound_route_mut)
            .map(mutate)
    }

    pub(super) fn with_inbound_route<T>(
        &self,
        token: ConnectionToken,
        inspect: impl FnOnce(&InboundRoute) -> T,
    ) -> Option<T> {
        self.slots
            .lookup_ref(token)
            .occupied()
            .and_then(ConnectionEntry::inbound_route)
            .map(inspect)
    }

    pub(super) fn with_inbound_route_mut<T>(
        &mut self,
        token: ConnectionToken,
        mutate: impl FnOnce(&mut InboundRoute) -> T,
    ) -> Option<T> {
        self.slots
            .get_mut(token)
            .and_then(ConnectionEntry::inbound_route_mut)
            .map(mutate)
    }

    pub(super) fn take_outbound_state_if(
        &mut self,
        token: ConnectionToken,
        predicate: impl FnOnce(&OutboundState) -> bool,
    ) -> Option<OutboundState> {
        self.with_outbound_route_mut(token, |route| route.take_state_if(predicate))
            .flatten()
    }

    pub(super) fn set_outbound_state(
        &mut self,
        token: ConnectionToken,
        state: OutboundState,
    ) -> bool {
        self.with_outbound_route_mut(token, |route| route.set_state(state))
            .is_some()
    }

    pub(super) fn take_inbound_state_if(
        &mut self,
        token: ConnectionToken,
        predicate: impl FnOnce(&InboundState) -> bool,
    ) -> Option<InboundState> {
        self.with_inbound_route_mut(token, |route| route.take_state_if(predicate))
            .flatten()
    }

    pub(super) fn set_inbound_state(
        &mut self,
        token: ConnectionToken,
        state: InboundState,
    ) -> bool {
        self.with_inbound_route_mut(token, |route| route.set_state(state))
            .is_some()
    }

    pub(super) fn set_route_identity(
        &mut self,
        token: ConnectionToken,
        raw_id: usize,
        context_key: usize,
    ) -> bool {
        if self
            .with_outbound_route_mut(token, |route| route.set_identity(raw_id, context_key))
            .is_some()
        {
            return true;
        }
        self.with_inbound_route_mut(token, |route| route.set_identity(raw_id, context_key))
            .is_some()
    }

    pub(super) fn inbound_listener(&self, token: ConnectionToken) -> Option<ListenerToken> {
        self.with_inbound_route(token, |route| route.listener)
    }

    pub(super) fn outbound_request(&self, token: ConnectionToken) -> Option<Arc<OutboundRequest>> {
        self.with_outbound_route(token, OutboundRoute::request)
            .flatten()
    }

    pub(super) fn outbound_is_establishing(&self, token: ConnectionToken) -> bool {
        self.with_outbound_route(token, OutboundRoute::is_establishing)
            .unwrap_or(false)
    }

    pub(super) fn outbound_is_disconnected(&self, token: ConnectionToken) -> bool {
        self.with_outbound_route(token, OutboundRoute::is_disconnected)
            .unwrap_or(false)
    }

    pub(super) fn connection_route(&self, token: ConnectionToken) -> Option<ConnectionCmRoute> {
        let entry = self.slots.lookup_ref(token).occupied()?;
        if entry.outbound_route().is_some() {
            Some(ConnectionCmRoute::Outbound(token.encode()))
        } else if entry.inbound_route().is_some() {
            Some(ConnectionCmRoute::Inbound(token.encode()))
        } else {
            None
        }
    }

    pub(super) fn mark_active(&mut self, token: ConnectionToken) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Outbound(OutboundConnectionEntry::Registered {
                route,
                connection,
            }) => (
                ConnectionEntry::Active(LiveConnectionEntry {
                    route: EstablishedRoute::Outbound(route),
                    connection,
                }),
                true,
            ),
            ConnectionEntry::Inbound(InboundConnectionEntry::Registered { route, connection }) => (
                ConnectionEntry::Active(LiveConnectionEntry {
                    route: EstablishedRoute::Inbound(route),
                    connection,
                }),
                true,
            ),
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn begin_close(&mut self, token: ConnectionToken) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Active(mut connection) => {
                connection.connection.begin_close();
                (ConnectionEntry::Draining(connection), true)
            }
            ConnectionEntry::Outbound(OutboundConnectionEntry::Registered {
                route,
                mut connection,
            }) => {
                connection.begin_close();
                (
                    ConnectionEntry::Draining(LiveConnectionEntry {
                        route: EstablishedRoute::Outbound(route),
                        connection,
                    }),
                    true,
                )
            }
            ConnectionEntry::Inbound(InboundConnectionEntry::Registered {
                route,
                mut connection,
            }) => {
                connection.begin_close();
                (
                    ConnectionEntry::Draining(LiveConnectionEntry {
                        route: EstablishedRoute::Inbound(route),
                        connection,
                    }),
                    true,
                )
            }
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(super) fn fail_close_into_quarantine(&mut self, token: ConnectionToken) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Draining(mut connection) => {
                connection.connection.rollback_draining_count();
                connection.connection.mark_reservation_quarantined();
                (
                    ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                        lifecycle: QuarantinedLifecycle::Draining(connection),
                        bundle: true,
                    }),
                    true,
                )
            }
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn request_retirement(&mut self, token: ConnectionToken) -> bool {
        let requested = self
            .transition(token, |entry| match entry {
                ConnectionEntry::Draining(connection)
                    if connection.connection.error_transition_complete() =>
                {
                    (
                        ConnectionEntry::Retiring {
                            connection,
                            started: false,
                        },
                        true,
                    )
                }
                other => (other, false),
            })
            .unwrap_or(false);
        if requested {
            self.retirements.push_back(token);
        }
        requested
    }

    pub(in crate::v2::engine) fn begin_retirement(&mut self, token: ConnectionToken) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Retiring {
                connection,
                started: false,
            } => (
                ConnectionEntry::Retiring {
                    connection,
                    started: true,
                },
                true,
            ),
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(super) fn retry_retirement(&mut self, token: ConnectionToken) {
        let retry = self
            .transition(token, |entry| match entry {
                ConnectionEntry::Retiring { connection, .. } => (
                    ConnectionEntry::Retiring {
                        connection,
                        started: false,
                    },
                    true,
                ),
                other => (other, false),
            })
            .unwrap_or(false);
        if retry {
            self.retirements.push_back(token);
        }
    }

    pub(in crate::v2::engine) fn retirement_is_quarantined(&self, token: ConnectionToken) -> bool {
        matches!(
            self.slots.lookup_ref(token),
            Lookup::Occupied(ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                lifecycle: QuarantinedLifecycle::Retiring { .. },
                bundle: true,
                ..
            }))
        )
    }

    pub(in crate::v2::engine) fn track_bundle_quarantine(
        &mut self,
        token: ConnectionToken,
    ) -> bool {
        self.transition_to_quarantine(token, true, None)
    }

    pub(super) fn retain_setup_quarantine(
        &mut self,
        token: ConnectionToken,
        poster: ConnectionPoster,
        mut reservation: ConnectionReservation,
    ) -> bool {
        reservation.retain_setup_quarantine();
        self.transition(token, |entry| match entry {
            ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(route)) => (
                ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                    lifecycle: QuarantinedLifecycle::OutboundSetup {
                        route,
                        _poster: poster,
                        reservation,
                    },
                    bundle: true,
                }),
                true,
            ),
            ConnectionEntry::Inbound(InboundConnectionEntry::Establishing(route)) => (
                ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                    lifecycle: QuarantinedLifecycle::InboundSetup {
                        route,
                        _poster: poster,
                        reservation,
                    },
                    bundle: true,
                }),
                true,
            ),
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(super) fn retain_detached_quarantine(
        &mut self,
        token: ConnectionToken,
        mut connection: ConnectionState,
    ) -> bool {
        connection.mark_reservation_quarantined();
        self.transition(token, |entry| match entry {
            ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(route)) => (
                ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                    lifecycle: QuarantinedLifecycle::Outbound(
                        OutboundConnectionEntry::Registered { route, connection },
                    ),
                    bundle: true,
                }),
                true,
            ),
            ConnectionEntry::Inbound(InboundConnectionEntry::Establishing(route)) => (
                ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                    lifecycle: QuarantinedLifecycle::Inbound(InboundConnectionEntry::Registered {
                        route,
                        connection,
                    }),
                    bundle: true,
                }),
                true,
            ),
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn track_operation_quarantine(
        &mut self,
        token: ConnectionToken,
        operation: OperationToken,
    ) -> bool {
        self.transition_to_quarantine(token, false, Some(operation))
    }

    fn transition_to_quarantine(
        &mut self,
        token: ConnectionToken,
        bundle: bool,
        operation: Option<OperationToken>,
    ) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Quarantined(mut quarantined) => {
                let first = if bundle {
                    !std::mem::replace(&mut quarantined.bundle, true)
                } else {
                    let Some(connection) = quarantined.lifecycle.connection_mut() else {
                        return (ConnectionEntry::Quarantined(quarantined), false);
                    };
                    connection.io_ledger.mark_operation_quarantined(
                        operation.expect("operation quarantine carries its exact token"),
                    )
                };
                if first && bundle {
                    if let Some(connection) = quarantined.lifecycle.connection_mut() {
                        connection.mark_reservation_quarantined();
                    } else if let Some(reservation) = quarantined.lifecycle.reservation_mut() {
                        reservation.retain_setup_quarantine();
                    }
                }
                (ConnectionEntry::Quarantined(quarantined), first)
            }
            other => {
                let lifecycle = match other {
                    ConnectionEntry::Outbound(entry) => QuarantinedLifecycle::Outbound(entry),
                    ConnectionEntry::Inbound(entry) => QuarantinedLifecycle::Inbound(entry),
                    ConnectionEntry::Active(entry) => QuarantinedLifecycle::Active(entry),
                    ConnectionEntry::Draining(entry) => QuarantinedLifecycle::Draining(entry),
                    ConnectionEntry::Retiring {
                        connection,
                        started,
                    } => QuarantinedLifecycle::Retiring {
                        connection,
                        started,
                    },
                    ConnectionEntry::Retired(entry) => {
                        return (ConnectionEntry::Retired(entry), false);
                    }
                    ConnectionEntry::Transitioning | ConnectionEntry::Quarantined(_) => {
                        unreachable!()
                    }
                };
                let mut quarantined = QuarantinedConnectionEntry { lifecycle, bundle };
                let first = if let Some(operation) = operation {
                    quarantined
                        .lifecycle
                        .connection_mut()
                        .is_some_and(|connection| {
                            connection.io_ledger.mark_operation_quarantined(operation)
                        })
                } else {
                    true
                };
                if bundle {
                    if let Some(connection) = quarantined.lifecycle.connection_mut() {
                        connection.mark_reservation_quarantined();
                    } else if let Some(reservation) = quarantined.lifecycle.reservation_mut() {
                        reservation.retain_setup_quarantine();
                    }
                }
                (ConnectionEntry::Quarantined(quarantined), first)
            }
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn clear_bundle_quarantine(
        &mut self,
        token: ConnectionToken,
    ) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Quarantined(mut quarantined) if quarantined.bundle => {
                quarantined.bundle = false;
                if let Some(connection) = quarantined.lifecycle.connection_mut() {
                    connection.recover_reservation_quarantine();
                }
                if quarantined
                    .lifecycle
                    .connection()
                    .is_some_and(|connection| {
                        connection.io_ledger.quarantined_operation_count() != 0
                    })
                {
                    (ConnectionEntry::Quarantined(quarantined), false)
                } else {
                    (Self::restore_quarantined(quarantined.lifecycle), true)
                }
            }
            other => (other, false),
        })
        .unwrap_or(false)
    }

    pub(in crate::v2::engine) fn clear_operation_quarantine(
        &mut self,
        token: ConnectionToken,
        operation: OperationToken,
    ) -> bool {
        self.transition(token, |entry| match entry {
            ConnectionEntry::Quarantined(mut quarantined) => {
                let Some(connection) = quarantined.lifecycle.connection_mut() else {
                    return (ConnectionEntry::Quarantined(quarantined), false);
                };
                if !connection.io_ledger.clear_operation_quarantined(operation) {
                    return (ConnectionEntry::Quarantined(quarantined), false);
                }
                if quarantined.bundle {
                    (ConnectionEntry::Quarantined(quarantined), false)
                } else {
                    (Self::restore_quarantined(quarantined.lifecycle), true)
                }
            }
            other => (other, false),
        })
        .unwrap_or(false)
    }

    fn restore_quarantined(lifecycle: QuarantinedLifecycle) -> ConnectionEntry {
        match lifecycle {
            lifecycle @ (QuarantinedLifecycle::OutboundSetup { .. }
            | QuarantinedLifecycle::InboundSetup { .. }) => {
                ConnectionEntry::Quarantined(QuarantinedConnectionEntry {
                    lifecycle,
                    bundle: true,
                })
            }
            QuarantinedLifecycle::Outbound(entry) => ConnectionEntry::Outbound(entry),
            QuarantinedLifecycle::Inbound(entry) => ConnectionEntry::Inbound(entry),
            QuarantinedLifecycle::Active(entry) => ConnectionEntry::Active(entry),
            QuarantinedLifecycle::Draining(entry) => ConnectionEntry::Draining(entry),
            QuarantinedLifecycle::Retiring {
                connection,
                started,
            } => ConnectionEntry::Retiring {
                connection,
                started,
            },
        }
    }

    fn transition<T>(
        &mut self,
        token: ConnectionToken,
        transition: impl FnOnce(ConnectionEntry) -> (ConnectionEntry, T),
    ) -> Option<T> {
        let entry = self.slots.get_mut(token)?;
        let current = std::mem::replace(entry, ConnectionEntry::Transitioning);
        let (next, output) = transition(current);
        debug_assert!(
            !matches!(next, ConnectionEntry::Transitioning),
            "a checked connection transition must restore one complete entry"
        );
        *entry = next;
        Some(output)
    }

    #[cfg(test)]
    pub(super) fn phase(&self, token: ConnectionToken) -> Option<ConnectionPhase> {
        self.slots
            .lookup_ref(token)
            .occupied()
            .map(ConnectionEntry::phase)
    }

    pub(in crate::v2::engine) fn release_unindexed(
        &mut self,
        token: ConnectionToken,
    ) -> Option<ConnectionState> {
        let entry = self.slots.release(token, true)?;
        if entry.outbound_route().is_some() || entry.inbound_route().is_some() {
            self.route_count = self.route_count.saturating_sub(1);
        }
        match entry {
            ConnectionEntry::Outbound(OutboundConnectionEntry::Registered {
                connection, ..
            })
            | ConnectionEntry::Inbound(InboundConnectionEntry::Registered { connection, .. }) => {
                Some(connection)
            }
            ConnectionEntry::Active(connection) | ConnectionEntry::Draining(connection) => {
                Some(connection.connection)
            }
            ConnectionEntry::Retired(connection) => Some(connection.connection),
            ConnectionEntry::Retiring { connection, .. } => Some(connection.connection),
            ConnectionEntry::Quarantined(quarantined) => match quarantined.lifecycle {
                QuarantinedLifecycle::Outbound(OutboundConnectionEntry::Registered {
                    connection,
                    ..
                })
                | QuarantinedLifecycle::Inbound(InboundConnectionEntry::Registered {
                    connection,
                    ..
                }) => Some(connection),
                QuarantinedLifecycle::Active(connection)
                | QuarantinedLifecycle::Draining(connection) => Some(connection.connection),
                QuarantinedLifecycle::Retiring { connection, .. } => Some(connection.connection),
                QuarantinedLifecycle::OutboundSetup { .. }
                | QuarantinedLifecycle::InboundSetup { .. } => None,
                QuarantinedLifecycle::Outbound(OutboundConnectionEntry::Establishing(_))
                | QuarantinedLifecycle::Inbound(InboundConnectionEntry::Establishing(_)) => None,
            },
            ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(_))
            | ConnectionEntry::Inbound(InboundConnectionEntry::Establishing(_))
            | ConnectionEntry::Transitioning => None,
        }
    }

    pub(super) fn release_route(&mut self, token: ConnectionToken, completed: bool) -> bool {
        let Some(entry) = self.slots.get_mut(token) else {
            return false;
        };
        if entry.connection().is_none() {
            let _ = self.slots.release(token, completed);
            self.route_count = self.route_count.saturating_sub(1);
            return true;
        }
        let removed = match entry {
            ConnectionEntry::Active(connection)
            | ConnectionEntry::Draining(connection)
            | ConnectionEntry::Retired(connection) => !matches!(
                std::mem::replace(&mut connection.route, EstablishedRoute::None),
                EstablishedRoute::None
            ),
            ConnectionEntry::Retiring { connection, .. } => !matches!(
                std::mem::replace(&mut connection.route, EstablishedRoute::None),
                EstablishedRoute::None
            ),
            ConnectionEntry::Quarantined(quarantined) => {
                if let Some(route) = quarantined.lifecycle.route_mut() {
                    !matches!(
                        std::mem::replace(route, EstablishedRoute::None),
                        EstablishedRoute::None
                    )
                } else {
                    false
                }
            }
            ConnectionEntry::Outbound(OutboundConnectionEntry::Registered { .. })
            | ConnectionEntry::Inbound(InboundConnectionEntry::Registered { .. }) => false,
            ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(_))
            | ConnectionEntry::Inbound(InboundConnectionEntry::Establishing(_))
            | ConnectionEntry::Transitioning => false,
        };
        if removed {
            self.route_count = self.route_count.saturating_sub(1);
        }
        removed
    }

    pub(in crate::v2::engine) fn lookup_qp(&self, qp_num: u32) -> Option<ConnectionToken> {
        self.qp_index.get(&qp_num).copied()
    }

    pub(in crate::v2::engine) fn prove_live_io(
        &self,
        connection: ConnectionToken,
        qp_num: u32,
    ) -> Option<LiveIoConnectionProof> {
        let entry = self.slots.lookup_ref(connection).occupied()?;
        let state = entry.connection()?;
        if state.qp_num() != qp_num || self.lookup_qp(qp_num) != Some(connection) {
            return None;
        }
        Some(LiveIoConnectionProof::issue_live_io_proof(
            connection, qp_num,
        ))
    }

    pub(in crate::v2::engine) fn release(
        &mut self,
        token: ConnectionToken,
        qp_num: u32,
    ) -> Option<ConnectionState> {
        if self.qp_index.get(&qp_num).copied() != Some(token) {
            return None;
        }
        let marked = self.transition(token, |entry| match entry {
            ConnectionEntry::Retiring {
                connection,
                started: true,
            } => (ConnectionEntry::Retired(connection), true),
            other => (other, false),
        })?;
        if !marked {
            return None;
        }
        self.qp_index.remove(&qp_num);
        self.release_unindexed(token)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn detach_qp_index(
        &mut self,
        token: ConnectionToken,
        qp_num: u32,
    ) -> bool {
        if self.qp_index.get(&qp_num).copied() != Some(token) {
            return false;
        }
        self.qp_index.remove(&qp_num);
        true
    }

    pub(in crate::v2::engine) fn live(&self) -> usize {
        self.slots.live()
    }

    pub(in crate::v2::engine) fn occupied(&self) -> Vec<ConnectionToken> {
        self.slots
            .occupied_tokens()
            .into_iter()
            .filter(|token| self.with_connection(*token, |_| ()).is_some())
            .collect()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn occupied_full_scans_for_test(&self) -> usize {
        self.slots.occupied_full_scans()
    }

    pub(in crate::v2::engine) fn scan_occupied(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<ConnectionToken>, usize, bool, usize) {
        let (tokens, next, complete, scanned) = self.slots.scan_occupied_tokens(start, budget);
        let connections = tokens
            .into_iter()
            .filter(|token| self.with_connection(*token, |_| ()).is_some())
            .collect();
        (connections, next, complete, scanned)
    }

    pub(super) fn scan_outbound_routes(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<ConnectionToken>, usize, bool, usize) {
        if self.route_count == 0 {
            return (Vec::new(), start, true, 0);
        }
        let (tokens, next, complete, scanned) = self.slots.scan_occupied_tokens(start, budget);
        let routes = tokens
            .into_iter()
            .filter(|token| self.with_outbound_route(*token, |_| ()).is_some())
            .collect();
        (routes, next, complete, scanned)
    }

    pub(super) fn outbound_routes(&self) -> Vec<ConnectionToken> {
        self.slots
            .occupied_tokens()
            .into_iter()
            .filter(|token| self.with_outbound_route(*token, |_| ()).is_some())
            .collect()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn set_qp_mapping_for_test(
        &mut self,
        qp_num: u32,
        token: ConnectionToken,
    ) {
        self.qp_index.insert(qp_num, token);
    }

    #[cfg(test)]
    fn invariant_snapshot(&self, token: ConnectionToken) -> Option<ConnectionInvariantSnapshot> {
        let entry = self.slots.lookup_ref(token).occupied()?;
        let connection = entry.connection();
        Some(ConnectionInvariantSnapshot {
            phase: entry.phase(),
            route: if entry.outbound_route().is_some() {
                Some(ConnectionRouteDirection::Outbound)
            } else if entry.inbound_route().is_some() {
                Some(ConnectionRouteDirection::Inbound)
            } else {
                None
            },
            raw_id: entry
                .outbound_route()
                .map(|route| route.raw_id)
                .or_else(|| entry.inbound_route().map(|route| route.raw_id)),
            context_key: entry
                .outbound_route()
                .map(|route| route.context_key)
                .or_else(|| entry.inbound_route().map(|route| route.context_key)),
            admission: entry.reservation().map(ConnectionReservation::state),
            qp_num: connection.map(ConnectionState::qp_num),
            qp_index: connection.and_then(|connection| self.lookup_qp(connection.qp_num())),
            io_identity: connection.map(|connection| connection.io_ledger.identity()),
            resource_owner: connection
                .map(ConnectionState::resource_owner_identity_for_test)
                .unwrap_or(0),
            accepted: connection.map_or(0, |connection| connection.io_ledger.accepted_count()),
            close_result: connection.and_then(ConnectionState::close_result_for_test),
            drained: connection.is_some_and(ConnectionState::drained_for_test),
            bundle_quarantined: matches!(
                entry,
                ConnectionEntry::Quarantined(QuarantinedConnectionEntry { bundle: true, .. })
            ),
            operation_quarantined: connection
                .is_some_and(|connection| connection.io_ledger.quarantined_operation_count() != 0),
        })
    }
}

#[cfg(test)]
struct ConnectionInvariantSnapshot {
    phase: ConnectionPhase,
    route: Option<ConnectionRouteDirection>,
    raw_id: Option<usize>,
    context_key: Option<usize>,
    admission: Option<super::connection::ReservationState>,
    qp_num: Option<u32>,
    qp_index: Option<ConnectionToken>,
    io_identity: Option<super::super::io_core::EstablishedIoIdentity>,
    resource_owner: usize,
    accepted: usize,
    close_result: Option<super::super::lifecycle::MemoizedTerminalResult>,
    drained: bool,
    bundle_quarantined: bool,
    operation_quarantined: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConnectionRouteDirection {
    Outbound,
    Inbound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConnectionRouteIdentity {
    pub(super) token: ConnectionToken,
    pub(super) raw_id: usize,
    pub(super) context_key: usize,
    pub(super) direction: ConnectionRouteDirection,
}

pub(in crate::v2::engine) struct ConnectionRegistrationFailure {
    pub(in crate::v2::engine) error: Error,
    pub(in crate::v2::engine) retained: Option<(ConnectionToken, ConnectionState)>,
}

impl std::fmt::Debug for ConnectionRegistrationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionRegistrationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

trait LookupOccupied<T> {
    fn occupied(self) -> Option<T>;
}

impl<T> LookupOccupied<T> for Lookup<T> {
    fn occupied(self) -> Option<T> {
        match self {
            Lookup::Occupied(value) => Some(value),
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::engine::RdmaConnectionConfig;
    use crate::v2::engine::session::connection::{ReservationState, TestConnectionProvider};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct TestPoster;

    impl TestConnectionProvider for TestPoster {
        fn qp_num(&self) -> u32 {
            41
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _: &mut PreparedSendBatch) -> crate::v2::Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _: &mut PreparedRecvBatch) -> crate::v2::Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn to_error(&self) -> crate::v2::Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) -> crate::v2::Result<bool> {
            Ok(true)
        }

        fn disconnect(&self) -> crate::v2::Result<()> {
            Ok(())
        }
    }

    fn outbound_registry() -> (ConnectionRegistry, ConnectionToken) {
        let mut registry = ConnectionRegistry::new(1).unwrap();
        let request = Arc::new(OutboundRequest::new(
            "127.0.0.1:7471".parse().unwrap(),
            RdmaConnectionConfig::default(),
            crate::v2::engine::session::listener::empty_connection_setup(),
        ));
        let reservation = registry.try_reserve().unwrap();
        let token = registry
            .register_outbound(|token| OutboundRoute::new(token, request))
            .unwrap();
        assert!(registry.set_route_identity(token, 11, 17));
        let connection = ConnectionState::new(
            token,
            Arc::new(TestPoster),
            RdmaConnectionConfig::default(),
            None,
            None,
            Some(reservation),
        );
        registry
            .attach_registered(token, 41, connection)
            .expect("attach exact QP owner");
        (registry, token)
    }

    #[test]
    fn diagnostics_remain_constant_time_after_registry_high_water() {
        let mut registry = ConnectionRegistry::new(2_048).unwrap();
        for slot in 0..1_025_u32 {
            registry
                .slots
                .allocate_owned(|token| {
                    debug_assert_eq!(token.slot, slot);
                    ConnectionEntry::Outbound(OutboundConnectionEntry::Establishing(
                        OutboundRoute::new(
                            token,
                            Arc::new(OutboundRequest::new(
                                "127.0.0.1:7471".parse().unwrap(),
                                RdmaConnectionConfig::default(),
                                crate::v2::engine::session::listener::empty_connection_setup(),
                            )),
                        ),
                    ))
                })
                .unwrap();
        }
        let scans_before = registry.occupied_full_scans_for_test();
        assert_eq!(
            registry.admission_snapshot(),
            ConnectionStateCountSnapshot::default()
        );
        assert_eq!(
            registry.admission_snapshot_excluding_retained(),
            ConnectionStateCountSnapshot::default()
        );
        assert_eq!(
            registry.occupied_full_scans_for_test(),
            scans_before,
            "ordinary diagnostics must not enumerate the registry high-water range"
        );
    }

    #[test]
    fn lifecycle_bundle_transitions_atomically_across_all_connection_facts() {
        let (mut registry, token) = outbound_registry();
        let active = registry.invariant_snapshot(token).unwrap();
        assert_eq!(active.phase, ConnectionPhase::Outbound);
        assert_eq!(active.route, Some(ConnectionRouteDirection::Outbound));
        assert_eq!((active.raw_id, active.context_key), (Some(11), Some(17)));
        assert_eq!(active.admission, Some(ReservationState::Established));
        assert_eq!(active.qp_num, Some(41));
        assert_eq!(active.qp_index, Some(token));
        assert_eq!(
            active.io_identity,
            Some(super::super::super::io_core::EstablishedIoIdentity {
                connection: token,
                qp_num: 41,
            })
        );
        assert_ne!(active.resource_owner, 0);
        assert_eq!(active.accepted, 0);
        assert!(active.close_result.is_none());
        assert!(!active.drained);
        assert!(!active.bundle_quarantined);
        assert!(!active.operation_quarantined);

        assert!(registry.mark_active(token));
        let operation = OperationToken {
            slot: 7,
            generation: 3,
        };
        registry
            .with_connection_io_mut(token, |connection, connection_io, _poster| {
                assert_eq!(connection.identity().connection, token);
                connection_io.insert_accepted_for_test(operation);
            })
            .unwrap();
        assert!(registry.begin_close(token));
        assert!(registry.mark_drained_once(token));
        registry
            .with_connection_mut(token, |connection| {
                connection.record_cm_failure(Error::TransportClosed);
                connection.transition_to_error_once().unwrap();
            })
            .unwrap();
        assert!(registry.request_retirement(token));
        assert!(registry.begin_retirement(token));
        assert!(registry.track_operation_quarantine(token, operation));
        assert!(registry.track_bundle_quarantine(token));

        let quarantined = registry.invariant_snapshot(token).unwrap();
        assert_eq!(quarantined.phase, ConnectionPhase::Quarantined);
        assert_eq!(quarantined.route, Some(ConnectionRouteDirection::Outbound));
        assert_eq!(quarantined.qp_index, Some(token));
        assert_eq!(quarantined.accepted, 1);
        assert!(quarantined.close_result.is_some());
        assert!(quarantined.drained);
        assert_eq!(quarantined.resource_owner, active.resource_owner);
        assert!(quarantined.bundle_quarantined);
        assert!(quarantined.operation_quarantined);
        assert_eq!(
            quarantined.admission,
            Some(ReservationState::QuarantinedDraining)
        );

        assert!(
            !registry.clear_operation_quarantine(token, operation),
            "bundle quarantine keeps the full entry quarantined"
        );
        assert!(registry.clear_bundle_quarantine(token));
        let retiring = registry.invariant_snapshot(token).unwrap();
        assert_eq!(retiring.phase, ConnectionPhase::Retiring);
        assert_eq!(retiring.accepted, 1);
        assert!(retiring.drained);
        assert_eq!(retiring.resource_owner, active.resource_owner);
        assert_eq!(retiring.admission, Some(ReservationState::Draining));
    }

    #[test]
    fn invalid_independent_transitions_are_rejected_without_partial_mutation() {
        let (mut registry, token) = outbound_registry();
        let before = registry.invariant_snapshot(token).unwrap();

        assert!(!registry.request_retirement(token));
        assert!(!registry.begin_retirement(token));
        assert!(!registry.clear_bundle_quarantine(token));
        assert!(!registry.publish_destroy_quarantine_for_test(
            ConnectionToken {
                slot: token.slot,
                generation: token.generation + 1,
            },
            &Error::TransportClosed,
        ));

        let after = registry.invariant_snapshot(token).unwrap();
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.route, before.route);
        assert_eq!(after.admission, before.admission);
        assert_eq!(after.qp_num, before.qp_num);
        assert_eq!(after.qp_index, before.qp_index);
        assert_eq!(after.resource_owner, before.resource_owner);
        assert_eq!(after.accepted, before.accepted);
        assert_eq!(after.bundle_quarantined, before.bundle_quarantined);
        assert_eq!(after.operation_quarantined, before.operation_quarantined);
    }

    #[test]
    fn exact_qp_index_and_live_proof_cannot_outlive_the_entry() {
        let (mut registry, token) = outbound_registry();
        assert!(registry.prove_live_io(token, 41).is_some());
        assert!(registry.mark_active(token));
        assert!(registry.begin_close(token));
        registry
            .with_connection_mut(token, |connection| {
                connection.transition_to_error_once().unwrap();
            })
            .unwrap();
        assert!(registry.request_retirement(token));
        assert!(registry.begin_retirement(token));
        let released = registry.release(token, 41).expect("exact release");
        assert_eq!(released.token, token);
        assert_eq!(released.io_ledger.accepted_count(), 0);
        assert!(registry.lookup_qp(41).is_none());
        assert!(registry.prove_live_io(token, 41).is_none());
        assert!(matches!(registry.lookup(token), Lookup::Duplicate));
    }

    #[test]
    fn route_direction_is_part_of_the_entry_not_a_parallel_owner() {
        let (registry, token) = outbound_registry();
        assert!(matches!(
            registry.lookup_outbound(token),
            Lookup::Occupied(ConnectionRouteIdentity {
                direction: ConnectionRouteDirection::Outbound,
                raw_id: 11,
                context_key: 17,
                ..
            })
        ));
        assert!(matches!(registry.lookup_inbound(token), Lookup::Unknown));
    }
}
