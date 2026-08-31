//! Shared RDMA-CM routing and outbound connection state machines.
//!
//! The engine driver is the only consumer of the shared CM event channel.
//! Every outbound ID owns an opaque context allocation indexed to a
//! non-wrapping route token. Events are copied into an identity snapshot and
//! acknowledged before state ownership advances or any potentially blocking
//! librdmacm/verbs call runs.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::connection::{
    ConnectionReservation, ConnectionState, SharedCmId, VerbsConnectionResources,
    WorkRequestPoster, install_reserved_connection, reserve_connection,
};
use super::diagnostics::CmEventReject;
use super::registry::{ConnectionToken, Lookup, PagedRegistry, RegistryToken, lock_unpoison};
use super::resources::EngineResources;
use super::{EngineOutcome, EngineShared, RdmaConnection, RdmaConnectionConfig};
use crate::cm::{CmEventType, CmId, PortSpace};
use crate::v2::error::{Error, Result};
use crate::v2::qp::QpBuilder;

pub(super) const CM_WORK: usize = 1 << 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct CmRouteToken {
    slot: u32,
    generation: u32,
}

impl CmRouteToken {
    const fn encode(self) -> u64 {
        ((self.generation as u64) << 32) | self.slot as u64
    }

    const fn decode(value: u64) -> Self {
        Self {
            slot: value as u32,
            generation: (value >> 32) as u32,
        }
    }
}

impl RegistryToken for CmRouteToken {
    fn from_parts(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    fn slot(self) -> u32 {
        self.slot
    }

    fn generation(self) -> u32 {
        self.generation
    }
}

pub(super) struct CmState {
    routes: PagedRegistry<CmRouteToken, Arc<OutboundRoute>>,
    context_routes: Mutex<HashMap<usize, CmRouteToken>>,
    pending: Mutex<VecDeque<Arc<OutboundRequest>>>,
    cancellations: Mutex<VecDeque<Arc<OutboundRequest>>>,
    retirements: Mutex<VecDeque<ConnectionToken>>,
    cm_destructions: Mutex<VecDeque<PendingCmDestruction>>,
    shutting_down: AtomicBool,
}

impl CmState {
    pub(super) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            routes: PagedRegistry::new(capacity)?,
            context_routes: Mutex::new(HashMap::new()),
            pending: Mutex::new(VecDeque::new()),
            cancellations: Mutex::new(VecDeque::new()),
            retirements: Mutex::new(VecDeque::new()),
            cm_destructions: Mutex::new(VecDeque::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn enqueue(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.pending).push_back(request);
    }

    fn defer_cm_id(&self, cm_id: SharedCmId) {
        lock_unpoison(&self.cm_destructions).push_back(PendingCmDestruction::Route(cm_id));
    }

    fn enqueue_cancellation(&self, request: Arc<OutboundRequest>) {
        if request.try_enqueue_cancellation() {
            lock_unpoison(&self.cancellations).push_back(request);
        }
    }

    pub(super) fn enqueue_retirement(&self, token: ConnectionToken) {
        lock_unpoison(&self.retirements).push_back(token);
    }

    fn mark_request_delivered(&self, request: &Arc<OutboundRequest>) {
        let encoded = request.route_token.load(Ordering::Acquire);
        if encoded == 0 {
            return;
        }
        if let Lookup::Occupied(route) = self.routes.lookup_cloned(CmRouteToken::decode(encoded)) {
            route.mark_delivered(request);
        }
    }

    fn insert_context_route(&self, context_key: usize, token: CmRouteToken) -> bool {
        match lock_unpoison(&self.context_routes).entry(context_key) {
            Entry::Vacant(entry) => {
                entry.insert(token);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    pub(super) fn has_software_work(&self) -> bool {
        !lock_unpoison(&self.pending).is_empty()
            || !lock_unpoison(&self.cancellations).is_empty()
            || !lock_unpoison(&self.retirements).is_empty()
            || !lock_unpoison(&self.cm_destructions).is_empty()
    }

    pub(super) fn service_software(
        &self,
        shared: &Arc<EngineShared>,
        resources: Option<&EngineResources>,
        budget: usize,
    ) -> Result<usize> {
        let mut remaining = [
            lock_unpoison(&self.cancellations).len(),
            lock_unpoison(&self.retirements).len(),
            lock_unpoison(&self.pending).len(),
        ];
        let mut next_class = 0;
        let mut processed = 0;
        while processed < budget && remaining.iter().any(|count| *count != 0) {
            let mut selected = None;
            for offset in 0..remaining.len() {
                let class = (next_class + offset) % remaining.len();
                if remaining[class] != 0 {
                    remaining[class] -= 1;
                    next_class = (class + 1) % remaining.len();
                    selected = Some(class);
                    break;
                }
            }
            let Some(class) = selected else {
                break;
            };
            match class {
                0 => {
                    let request = { lock_unpoison(&self.cancellations).pop_front() };
                    if let Some(request) = request {
                        self.process_cancellation(shared, request)?;
                        processed += 1;
                    }
                }
                1 => {
                    let token = { lock_unpoison(&self.retirements).pop_front() };
                    if let Some(token) = token {
                        self.retire_registered_connection(shared, token)?;
                        processed += 1;
                    }
                }
                2 => {
                    let request = { lock_unpoison(&self.pending).pop_front() };
                    if let Some(request) = request {
                        let resources = resources.ok_or_else(|| {
                            Error::InvalidConfig(
                                "CM pending work requires live engine resources".into(),
                            )
                        })?;
                        self.start_outbound(shared, resources, request)?;
                        processed += 1;
                    }
                }
                _ => unreachable!("software work has three classes"),
            }
        }
        Ok(processed)
    }

    pub(super) fn try_process_event(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
    ) -> Result<bool> {
        let event = match resources.cm_event_channel.try_get_event() {
            Ok(event) => event,
            Err(crate::Error::WouldBlock) => return Ok(false),
            Err(crate::Error::Verbs(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let snapshot = CmEventSnapshot {
            event_type: event.event_type(),
            status: event.status(),
            id: event.cm_id_raw() as usize,
            listen_id: event.listen_id_raw() as usize,
            context_key: event.context_key(),
        };
        let route = self.lookup_event_route(snapshot);
        event.ack_checked().map_err(Error::from)?;
        shared
            .diagnostic_counters
            .cm_events_processed
            .fetch_add(1, Ordering::Relaxed);

        let route = match route {
            Ok(route) => route,
            Err(reject) => {
                shared.diagnostic_counters.reject_cm_event(reject);
                return Ok(true);
            }
        };
        match self.handle_event(shared, resources, &route, snapshot)? {
            EventDisposition::Handled => {}
            EventDisposition::Rejected(reject) => {
                shared.diagnostic_counters.reject_cm_event(reject);
            }
        }
        Ok(true)
    }

    pub(super) fn begin_shutdown(&self, outcome: &EngineOutcome) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        if matches!(outcome, EngineOutcome::Success) {
            return;
        }
        let pending: Vec<_> = lock_unpoison(&self.pending).drain(..).collect();
        for request in pending {
            request.cancel(terminal_error(outcome));
            drop(request.take_reservation());
        }
        let requests: Vec<_> = self
            .routes
            .occupied_cloned()
            .into_iter()
            .filter_map(|route| route.request())
            .collect();
        for request in requests {
            request.cancel(terminal_error(outcome));
            self.enqueue_cancellation(request);
        }
    }

    pub(super) fn pending_route_count(&self) -> usize {
        let establishing = self
            .routes
            .occupied_cloned()
            .into_iter()
            .filter(|route| route.is_establishing())
            .count();
        establishing
            + lock_unpoison(&self.pending).len()
            + lock_unpoison(&self.cancellations).len()
            + lock_unpoison(&self.retirements).len()
            + lock_unpoison(&self.cm_destructions).len()
    }

    pub(super) fn retained_owner_count(&self) -> usize {
        self.routes.live() + lock_unpoison(&self.cm_destructions).len()
    }

    pub(super) fn service_cm_destructions(
        &self,
        shared: &Arc<EngineShared>,
        budget: usize,
        mut try_process_event: impl FnMut() -> Result<bool>,
    ) -> Result<usize> {
        let mut processed = 0;
        while processed < budget {
            let pending = { lock_unpoison(&self.cm_destructions).pop_front() };
            let Some(pending) = pending else {
                break;
            };
            match try_process_event() {
                Ok(true) => {
                    lock_unpoison(&self.cm_destructions).push_back(pending);
                }
                Ok(false) => {
                    self.remove_context_route(pending.cm_id());
                    match pending {
                        PendingCmDestruction::Route(cm_id) => cm_id.destroy()?,
                        PendingCmDestruction::Connection { cm_id, connection } => {
                            cm_id.destroy()?;
                            self.finalize_connection_retirement(shared, connection)?;
                        }
                        #[cfg(test)]
                        PendingCmDestruction::Test { destroyed } => {
                            destroyed.store(true, Ordering::Release);
                        }
                    }
                }
                Err(error) => {
                    lock_unpoison(&self.cm_destructions).push_front(pending);
                    return Err(error);
                }
            }
            processed += 1;
        }
        Ok(processed)
    }

    pub(super) fn terminalize(&self, outcome: &EngineOutcome) {
        if matches!(outcome, EngineOutcome::Success) {
            return;
        }
        let pending: Vec<_> = lock_unpoison(&self.pending).drain(..).collect();
        let requests: Vec<_> = self
            .routes
            .occupied_cloned()
            .into_iter()
            .filter_map(|route| route.request())
            .chain(pending)
            .collect();
        for request in requests {
            drop(request.take_reservation());
            request.cancel(terminal_error(outcome));
        }
    }

    fn start_outbound(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        request: Arc<OutboundRequest>,
    ) -> Result<()> {
        if request.cancelled.load(Ordering::Acquire) || self.shutting_down.load(Ordering::Acquire) {
            request.take_reservation();
            request.complete(Err(Error::DriverShutdown));
            return Ok(());
        }
        let reservation = request.take_reservation().ok_or_else(|| {
            Error::InvalidConfig("outbound request lost its connection reservation".into())
        })?;
        let (token, route) = match self
            .routes
            .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
        {
            Ok(route) => route,
            Err(error) => {
                drop(reservation);
                request.complete_failure(shared, error);
                return Ok(());
            }
        };
        request.route_token.store(token.encode(), Ordering::Release);

        let cm_id = match CmId::new_with_context_token(
            &resources.cm_event_channel,
            PortSpace::Tcp,
            token.encode(),
        ) {
            Ok(cm_id) => SharedCmId::new(cm_id, Arc::clone(&resources.cm_event_channel)),
            Err(error) => {
                self.routes.release(token, false);
                drop(reservation);
                request.complete_failure(shared, error.into());
                return Ok(());
            }
        };
        let Some(context_token) = cm_id.context_token() else {
            self.defer_cm_id(cm_id);
            self.routes.release(token, false);
            drop(reservation);
            request.complete_failure(
                shared,
                Error::InvalidConfig("engine CM ID lost its route context token".into()),
            );
            return Ok(());
        };
        let context_route = CmRouteToken::decode(context_token);
        if context_route != token {
            self.defer_cm_id(cm_id);
            self.routes.release(token, false);
            drop(reservation);
            request.complete_failure(
                shared,
                Error::InvalidConfig("engine CM context token did not match its route".into()),
            );
            return Ok(());
        }
        let context_key = cm_id.context_key();
        route.set_identity(cm_id.as_raw() as usize, context_key);
        if !self.insert_context_route(context_key, context_route) {
            self.defer_cm_id(cm_id);
            self.routes.release(token, false);
            drop(reservation);
            request.complete_failure(
                shared,
                Error::InvalidConfig("duplicate CM context identity".into()),
            );
            return Ok(());
        }

        let resolve = cm_id.resolve_addr(None, &request.address, 2_000);
        match resolve {
            Ok(()) => route.set_state(OutboundState::AwaitAddr {
                cm_id,
                request,
                reservation,
            }),
            Err(error) => {
                self.defer_cm_id(cm_id);
                self.retire_route(&route, false);
                drop(reservation);
                request.complete_failure(shared, error.into());
            }
        }
        Ok(())
    }

    fn process_cancellation(
        &self,
        shared: &Arc<EngineShared>,
        request: Arc<OutboundRequest>,
    ) -> Result<()> {
        let encoded = request.route_token.load(Ordering::Acquire);
        if encoded == 0 {
            return Ok(());
        }
        let token = CmRouteToken::decode(encoded);
        let Lookup::Occupied(route) = self.routes.lookup_cloned(token) else {
            return Ok(());
        };
        let state = route.take_state_if(|state| {
            matches!(
                state,
                OutboundState::EstablishedAwaitingDelivery { .. }
                    | OutboundState::DisconnectedAwaitingDelivery { .. }
                    | OutboundState::FailedAwaitingDelivery { .. }
            )
        });
        let Some(state) = state else {
            return Ok(());
        };
        let (route_request, connection) = match state {
            OutboundState::EstablishedAwaitingDelivery {
                request,
                connection,
            }
            | OutboundState::DisconnectedAwaitingDelivery {
                request,
                connection,
            }
            | OutboundState::FailedAwaitingDelivery {
                request,
                connection,
            } => (request, connection),
            _ => unreachable!("cancellation state was pre-filtered"),
        };
        debug_assert!(Arc::ptr_eq(&route_request, &request));
        let Some(connection_state) = connection.upgrade() else {
            self.retire_route(&route, true);
            return Ok(());
        };
        route.set_state(OutboundState::Closing {
            connection: connection.clone(),
        });
        shared.begin_connection_close(&connection_state, true);
        drop(request.take_result());
        if connection_state.accepted_count() == 0 {
            self.retire_registered_connection(shared, connection_state.token)?;
        }
        drop(route_request);
        Ok(())
    }

    fn retire_registered_connection(
        &self,
        shared: &EngineShared,
        token: ConnectionToken,
    ) -> Result<()> {
        let Lookup::Occupied(connection) = shared.connections.lookup(token) else {
            return Ok(());
        };
        if connection.accepted_count() != 0 || !connection.try_begin_retirement() {
            return Ok(());
        }
        if let Some(encoded) = connection.outbound_route_token() {
            match self.retire_connection_route(encoded, &connection)? {
                RouteRetirement::Complete => {}
                RouteRetirement::Retry => {
                    connection.retry_retirement();
                    self.enqueue_retirement(token);
                    return Ok(());
                }
            }
        }
        let cm_id = connection.destroy_connection_resources();
        if let Some(cm_id) = cm_id {
            lock_unpoison(&self.cm_destructions)
                .push_back(PendingCmDestruction::Connection { cm_id, connection });
            return Ok(());
        }
        self.finalize_connection_retirement(shared, connection)
    }

    fn finalize_connection_retirement(
        &self,
        shared: &EngineShared,
        connection: Arc<ConnectionState>,
    ) -> Result<()> {
        let released = shared
            .connections
            .release(connection.token, connection.qp_num())
            .ok_or_else(|| {
                Error::InvalidConfig("connection registry retirement lost its entry".into())
            })?;
        if !Arc::ptr_eq(&released, &connection) {
            return Err(Error::InvalidConfig(
                "connection registry retired a mismatched generation".into(),
            ));
        }
        connection.release_admission();
        connection.finish_retirement();
        Ok(())
    }

    fn retire_connection_route(
        &self,
        encoded: u64,
        connection: &Arc<ConnectionState>,
    ) -> Result<RouteRetirement> {
        let token = CmRouteToken::decode(encoded);
        let route = match self.routes.lookup_cloned(token) {
            Lookup::Occupied(route) => route,
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                return Ok(RouteRetirement::Complete);
            }
        };
        let state = route.take_state_if(|state| state.references_connection(connection.token));
        match state {
            Some(
                OutboundState::EstablishedAwaitingDelivery { .. }
                | OutboundState::Established { .. }
                | OutboundState::DisconnectedAwaitingDelivery { .. }
                | OutboundState::Disconnected { .. }
                | OutboundState::FailedAwaitingDelivery { .. }
                | OutboundState::Failed { .. }
                | OutboundState::Closing { .. },
            ) => {
                self.retire_route(&route, true);
                Ok(RouteRetirement::Complete)
            }
            Some(state) => {
                route.set_state(state);
                Err(Error::InvalidConfig(
                    "connection route was not established during retirement".into(),
                ))
            }
            None if matches!(&*lock_unpoison(&route.state), OutboundState::Transitioning) => {
                Ok(RouteRetirement::Retry)
            }
            None => Err(Error::InvalidConfig(
                "connection route generation did not match retirement".into(),
            )),
        }
    }

    fn lookup_event_route(
        &self,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<Arc<OutboundRoute>, CmEventReject> {
        if snapshot.context_key == 0 {
            return Err(CmEventReject::Unknown);
        }
        let token = lock_unpoison(&self.context_routes)
            .get(&snapshot.context_key)
            .copied()
            .ok_or(CmEventReject::Unknown)?;
        let route = match self.routes.lookup_cloned(token) {
            Lookup::Occupied(route) => route,
            Lookup::Duplicate => return Err(CmEventReject::Duplicate),
            Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
            Lookup::Unknown => return Err(CmEventReject::Unknown),
        };
        if route.raw_id.load(Ordering::Acquire) != snapshot.id {
            return Err(CmEventReject::WrongId);
        }
        Ok(route)
    }

    fn handle_event(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        route: &Arc<OutboundRoute>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        if is_failure_event(snapshot.event_type) || snapshot.status != 0 {
            return self.handle_failure_event(shared, route, snapshot);
        }
        match snapshot.event_type {
            CmEventType::AddrResolved => self.handle_addr_resolved(shared, resources, route),
            CmEventType::RouteResolved => self.handle_route_resolved(shared, resources, route),
            CmEventType::Established => self.handle_established(shared, route),
            CmEventType::Disconnected => self.handle_disconnected(shared, route),
            CmEventType::TimewaitExit => {
                if route.is_disconnected() {
                    Ok(EventDisposition::Handled)
                } else {
                    Ok(EventDisposition::Rejected(CmEventReject::Unexpected))
                }
            }
            _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
        }
    }

    fn handle_addr_resolved(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        route: &Arc<OutboundRoute>,
    ) -> Result<EventDisposition> {
        let Some(OutboundState::AwaitAddr {
            cm_id,
            request,
            reservation,
        }) = route.take_state_if(|state| matches!(state, OutboundState::AwaitAddr { .. }))
        else {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        };
        if request.cancelled.load(Ordering::Acquire)
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            self.defer_cm_id(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete(Err(Error::DriverShutdown));
            return Ok(EventDisposition::Handled);
        }
        if let Err(error) = cm_id.require_context(resources.context.inner()) {
            self.defer_cm_id(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete_failure(shared, error.into());
            return Ok(EventDisposition::Handled);
        }
        match cm_id.resolve_route(2_000) {
            Ok(()) => route.set_state(OutboundState::AwaitRoute {
                cm_id,
                request,
                reservation,
            }),
            Err(error) => {
                self.defer_cm_id(cm_id);
                drop(reservation);
                self.retire_route(route, true);
                request.complete_failure(shared, error.into());
            }
        }
        Ok(EventDisposition::Handled)
    }

    fn handle_route_resolved(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        route: &Arc<OutboundRoute>,
    ) -> Result<EventDisposition> {
        let Some(OutboundState::AwaitRoute {
            cm_id,
            request,
            reservation,
        }) = route.take_state_if(|state| matches!(state, OutboundState::AwaitRoute { .. }))
        else {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        };
        if request.cancelled.load(Ordering::Acquire)
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            self.defer_cm_id(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete(Err(Error::DriverShutdown));
            return Ok(EventDisposition::Handled);
        }
        if let Err(error) = cm_id.require_context(resources.context.inner()) {
            self.defer_cm_id(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete_failure(shared, error.into());
            return Ok(EventDisposition::Handled);
        }

        let local_addr = cm_id.local_addr();
        let peer_addr = cm_id.peer_addr();
        let qp = match build_qp(resources, &cm_id, &request.config) {
            Ok(qp) => qp,
            Err(error) => {
                self.defer_cm_id(cm_id);
                drop(reservation);
                self.retire_route(route, true);
                request.complete_failure(shared, error);
                return Ok(EventDisposition::Handled);
            }
        };
        let verbs = Arc::new(VerbsConnectionResources::new_shared(qp, cm_id));
        let connection = match install_reserved_connection(
            shared,
            Arc::clone(&verbs) as Arc<_>,
            request.config.clone(),
            local_addr,
            peer_addr,
            reservation,
            Some(route.token.encode()),
        ) {
            Ok(connection) => connection,
            Err(error) => {
                if let Some(cm_id) = verbs.destroy_connection() {
                    self.defer_cm_id(cm_id);
                }
                drop(verbs);
                self.retire_route(route, true);
                request.complete_failure(shared, error);
                return Ok(EventDisposition::Handled);
            }
        };

        let Some(setup) = request.take_setup() else {
            drop(verbs);
            self.fail_registered_connection(
                shared,
                route,
                request,
                connection,
                Error::InvalidConfig("outbound request setup was consumed more than once".into()),
            )?;
            return Ok(EventDisposition::Handled);
        };
        let conn_param = match request.config.conn_param() {
            Ok(param) => param,
            Err(error) => {
                drop(verbs);
                self.fail_registered_connection(shared, route, request, connection, error)?;
                return Ok(EventDisposition::Handled);
            }
        };
        let establish = run_setup_before_connect(
            setup,
            shared,
            &connection,
            || {
                if request.cancelled.load(Ordering::Acquire)
                    || shared.shutdown_requested.load(Ordering::Acquire)
                {
                    Err(Error::DriverShutdown)
                } else {
                    Ok(())
                }
            },
            || verbs.connect(&conn_param),
        );
        if let Err(error) = establish {
            drop(verbs);
            self.fail_registered_connection(shared, route, request, connection, error)?;
            return Ok(EventDisposition::Handled);
        }
        if request.cancelled.load(Ordering::Acquire)
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            drop(verbs);
            self.fail_registered_connection(
                shared,
                route,
                request,
                connection,
                Error::DriverShutdown,
            )?;
            return Ok(EventDisposition::Handled);
        }
        drop(verbs);
        route.set_state(OutboundState::AwaitEstablished {
            request,
            connection,
        });
        Ok(EventDisposition::Handled)
    }

    fn fail_registered_connection(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<OutboundRoute>,
        request: Arc<OutboundRequest>,
        connection: RdmaConnection,
        error: Error,
    ) -> Result<()> {
        let connection_state = Arc::clone(&connection.state);
        route.set_state(OutboundState::Closing {
            connection: EstablishedConnectionRoute::new(&connection_state),
        });
        shared.begin_connection_close(&connection_state, true);
        drop(connection);
        if connection_state.accepted_count() == 0 {
            self.retire_registered_connection(shared, connection_state.token)?;
        }
        if matches!(&error, Error::DriverShutdown) {
            request.complete(Err(error));
        } else {
            request.complete_failure(shared, error);
        }
        Ok(())
    }

    fn handle_established(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<OutboundRoute>,
    ) -> Result<EventDisposition> {
        let Some(OutboundState::AwaitEstablished {
            request,
            connection,
        }) = route.take_state_if(|state| matches!(state, OutboundState::AwaitEstablished { .. }))
        else {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        };
        if request.cancelled.load(Ordering::Acquire)
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            self.fail_registered_connection(
                shared,
                route,
                request,
                connection,
                Error::DriverShutdown,
            )?;
            return Ok(EventDisposition::Handled);
        }
        let waiter = Arc::clone(&request);
        route.set_state(OutboundState::EstablishedAwaitingDelivery {
            request,
            connection: EstablishedConnectionRoute::new(&connection.state),
        });
        shared
            .diagnostic_counters
            .connections_opened
            .fetch_add(1, Ordering::Relaxed);
        waiter.complete(Ok(connection));
        Ok(EventDisposition::Handled)
    }

    fn handle_disconnected(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<OutboundRoute>,
    ) -> Result<EventDisposition> {
        let state = route.take_state_if(|state| {
            matches!(
                state,
                OutboundState::EstablishedAwaitingDelivery { .. }
                    | OutboundState::Established { .. }
            )
        });
        let Some(state) = state else {
            if route.is_disconnected() {
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
        };
        let (request, connection) = match state {
            OutboundState::EstablishedAwaitingDelivery {
                request,
                connection,
            } => (Some(request), connection),
            OutboundState::Established { connection } => (None, connection),
            _ => unreachable!("disconnect state was pre-filtered"),
        };
        let Some(connection_state) = connection.upgrade() else {
            self.retire_route(route, true);
            return Ok(EventDisposition::Handled);
        };
        connection_state.mark_disconnected();
        let awaiting_delivery = request
            .as_ref()
            .is_some_and(|request| !request.delivered.load(Ordering::Acquire));
        if awaiting_delivery {
            route.set_state(OutboundState::DisconnectedAwaitingDelivery {
                request: request.expect("awaiting delivery retains its request"),
                connection: connection.clone(),
            });
        } else {
            route.set_state(OutboundState::Disconnected {
                connection: connection.clone(),
            });
        }
        shared.begin_connection_close(&connection_state, true);
        if connection_state.accepted_count() == 0 {
            self.retire_registered_connection(shared, connection_state.token)?;
        }
        Ok(EventDisposition::Handled)
    }

    fn handle_failure_event(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<OutboundRoute>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        let message = format!(
            "RDMA CM {:?} failed with status {} for id={:#x} listen_id={:#x}",
            snapshot.event_type, snapshot.status, snapshot.id, snapshot.listen_id
        );
        let state = route.take_state_if(|_| true);
        let Some(state) = state else {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        };
        match state {
            OutboundState::AwaitAddr {
                cm_id,
                request,
                reservation,
            }
            | OutboundState::AwaitRoute {
                cm_id,
                request,
                reservation,
            } => {
                self.defer_cm_id(cm_id);
                drop(reservation);
                self.retire_route(route, true);
                request.complete_failure(shared, Error::Verbs(std::io::Error::other(message)));
            }
            OutboundState::AwaitEstablished {
                request,
                connection,
            } => {
                self.fail_registered_connection(
                    shared,
                    route,
                    request,
                    connection,
                    Error::Verbs(std::io::Error::other(message)),
                )?;
            }
            OutboundState::EstablishedAwaitingDelivery {
                request,
                connection,
            }
            | OutboundState::DisconnectedAwaitingDelivery {
                request,
                connection,
            } => {
                request.record_failure(shared);
                let Some(connection_state) = connection.upgrade() else {
                    self.retire_route(route, true);
                    return Ok(EventDisposition::Handled);
                };
                connection_state.mark_cm_failure(message);
                if request.delivered.load(Ordering::Acquire) {
                    route.set_state(OutboundState::Failed {
                        connection: connection.clone(),
                    });
                } else {
                    route.set_state(OutboundState::FailedAwaitingDelivery {
                        request,
                        connection: connection.clone(),
                    });
                }
                shared.begin_connection_close(&connection_state, true);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            OutboundState::Established { connection }
            | OutboundState::Disconnected { connection } => {
                shared
                    .diagnostic_counters
                    .connections_failed
                    .fetch_add(1, Ordering::Relaxed);
                let Some(connection_state) = connection.upgrade() else {
                    self.retire_route(route, true);
                    return Ok(EventDisposition::Handled);
                };
                connection_state.mark_cm_failure(message);
                route.set_state(OutboundState::Failed {
                    connection: connection.clone(),
                });
                shared.begin_connection_close(&connection_state, true);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            OutboundState::FailedAwaitingDelivery {
                request,
                connection,
            } => {
                route.set_state(OutboundState::FailedAwaitingDelivery {
                    request,
                    connection,
                });
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            OutboundState::Failed { connection } => {
                route.set_state(OutboundState::Failed { connection });
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            OutboundState::Closing { connection } => {
                route.set_state(OutboundState::Closing { connection });
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            OutboundState::Transitioning => {
                return Err(Error::InvalidConfig(
                    "CM route was re-entered while transitioning".into(),
                ));
            }
        }
        Ok(EventDisposition::Handled)
    }

    fn retire_route(&self, route: &Arc<OutboundRoute>, completed: bool) {
        self.routes.release(route.token, completed);
    }

    fn remove_context_route(&self, cm_id: Option<&SharedCmId>) {
        let Some(cm_id) = cm_id else {
            return;
        };
        let Some(encoded) = cm_id.context_token() else {
            return;
        };
        let token = CmRouteToken::decode(encoded);
        let context_key = cm_id.context_key();
        let mut contexts = lock_unpoison(&self.context_routes);
        if contexts.get(&context_key).copied() == Some(token) {
            contexts.remove(&context_key);
        }
    }
}

pub(super) async fn connect(
    shared: Arc<EngineShared>,
    address: SocketAddr,
    config: RdmaConnectionConfig,
) -> Result<RdmaConnection> {
    connect_with_setup(shared, address, config, Box::new(EmptyPreEstablishSetup)).await
}

pub(super) async fn connect_with_setup(
    shared: Arc<EngineShared>,
    address: SocketAddr,
    config: RdmaConnectionConfig,
    setup: Box<dyn PreEstablishSetup>,
) -> Result<RdmaConnection> {
    config.validate(&shared.config, shared.provider.as_ref())?;
    let reservation = reserve_connection(&shared)?;
    let request = Arc::new(OutboundRequest::new(address, config, setup, reservation));
    shared.cm.enqueue(Arc::clone(&request));
    shared.work_signal.publish(CM_WORK);
    ConnectWaiter {
        shared,
        request,
        finished: false,
    }
    .await
}

pub(super) trait PreEstablishSetup: Send {
    fn run(
        self: Box<Self>,
        shared: &Arc<EngineShared>,
        connection: &RdmaConnection,
    ) -> Result<SetupSummary>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SetupSummary {
    pub(super) posted_wrs: usize,
}

struct EmptyPreEstablishSetup;

impl PreEstablishSetup for EmptyPreEstablishSetup {
    fn run(
        self: Box<Self>,
        _shared: &Arc<EngineShared>,
        _connection: &RdmaConnection,
    ) -> Result<SetupSummary> {
        Ok(SetupSummary { posted_wrs: 0 })
    }
}

fn run_setup_before_connect(
    setup: Box<dyn PreEstablishSetup>,
    shared: &Arc<EngineShared>,
    connection: &RdmaConnection,
    before_connect: impl FnOnce() -> Result<()>,
    connect: impl FnOnce() -> Result<()>,
) -> Result<SetupSummary> {
    let accepted_before = connection.state.accepted_count();
    let summary = setup.run(shared, connection)?;
    let accepted_after = connection.state.accepted_count();
    let posted_wrs = accepted_after.checked_sub(accepted_before).ok_or_else(|| {
        Error::InvalidConfig("pre-establishment setup reduced the accepted WR set".into())
    })?;
    if posted_wrs != summary.posted_wrs {
        return Err(Error::InvalidConfig(format!(
            "pre-establishment setup reported {} posted WRs but registered {posted_wrs}",
            summary.posted_wrs
        )));
    }
    before_connect()?;
    connect()?;
    Ok(summary)
}

fn build_qp(
    resources: &EngineResources,
    cm_id: &CmId,
    config: &RdmaConnectionConfig,
) -> Result<crate::v2::Qp> {
    QpBuilder::new(&resources.pd, &resources.cq, &resources.cq)
        .max_send_wr(
            u32::try_from(config.maximum_send_work_requests())
                .map_err(|_| Error::InvalidConfig("maximum send WRs do not fit u32".into()))?,
        )
        .max_recv_wr(
            u32::try_from(config.maximum_receive_work_requests())
                .map_err(|_| Error::InvalidConfig("maximum receive WRs do not fit u32".into()))?,
        )
        .max_send_sge(
            u32::try_from(config.maximum_send_sges())
                .map_err(|_| Error::InvalidConfig("maximum send SGEs do not fit u32".into()))?,
        )
        .max_recv_sge(
            u32::try_from(config.maximum_receive_sges())
                .map_err(|_| Error::InvalidConfig("maximum receive SGEs do not fit u32".into()))?,
        )
        .sq_sig_all(true)
        .build_with_cm(cm_id)
}

fn is_failure_event(event: CmEventType) -> bool {
    matches!(
        event,
        CmEventType::AddrError
            | CmEventType::RouteError
            | CmEventType::ConnectError
            | CmEventType::Unreachable
            | CmEventType::Rejected
            | CmEventType::DeviceRemoval
            | CmEventType::AddrChange
    )
}

fn terminal_error(outcome: &EngineOutcome) -> Error {
    match outcome.clone().into_result() {
        Err(error) => error,
        Ok(()) => unreachable!("successful engine outcome was filtered"),
    }
}

#[derive(Clone, Copy)]
struct CmEventSnapshot {
    event_type: CmEventType,
    status: i32,
    id: usize,
    listen_id: usize,
    context_key: usize,
}

enum EventDisposition {
    Handled,
    Rejected(CmEventReject),
}

enum RouteRetirement {
    Complete,
    Retry,
}

enum PendingCmDestruction {
    Route(SharedCmId),
    Connection {
        cm_id: SharedCmId,
        connection: Arc<ConnectionState>,
    },
    #[cfg(test)]
    Test {
        destroyed: Arc<AtomicBool>,
    },
}

impl PendingCmDestruction {
    fn cm_id(&self) -> Option<&SharedCmId> {
        match self {
            Self::Route(cm_id) | Self::Connection { cm_id, .. } => Some(cm_id),
            #[cfg(test)]
            Self::Test { .. } => None,
        }
    }
}

struct OutboundRoute {
    token: CmRouteToken,
    raw_id: AtomicUsize,
    context_key: AtomicUsize,
    state: Mutex<OutboundState>,
}

impl OutboundRoute {
    fn new(token: CmRouteToken, request: Arc<OutboundRequest>) -> Self {
        Self {
            token,
            raw_id: AtomicUsize::new(0),
            context_key: AtomicUsize::new(0),
            state: Mutex::new(OutboundState::Transitioning),
        }
        .with_initial_request(request)
    }

    fn with_initial_request(self, request: Arc<OutboundRequest>) -> Self {
        request
            .route_token
            .store(self.token.encode(), Ordering::Release);
        self
    }

    fn set_identity(&self, raw_id: usize, context_key: usize) {
        self.raw_id.store(raw_id, Ordering::Release);
        self.context_key.store(context_key, Ordering::Release);
    }

    fn set_state(&self, state: OutboundState) {
        *lock_unpoison(&self.state) = state;
    }

    fn take_state_if(
        &self,
        predicate: impl FnOnce(&OutboundState) -> bool,
    ) -> Option<OutboundState> {
        let mut state = lock_unpoison(&self.state);
        if !predicate(&state) {
            return None;
        }
        Some(std::mem::replace(&mut *state, OutboundState::Transitioning))
    }

    fn request(&self) -> Option<Arc<OutboundRequest>> {
        match &*lock_unpoison(&self.state) {
            OutboundState::AwaitAddr { request, .. }
            | OutboundState::AwaitRoute { request, .. }
            | OutboundState::AwaitEstablished { request, .. }
            | OutboundState::EstablishedAwaitingDelivery { request, .. }
            | OutboundState::DisconnectedAwaitingDelivery { request, .. }
            | OutboundState::FailedAwaitingDelivery { request, .. } => Some(Arc::clone(request)),
            OutboundState::Established { .. }
            | OutboundState::Disconnected { .. }
            | OutboundState::Failed { .. }
            | OutboundState::Closing { .. }
            | OutboundState::Transitioning => None,
        }
    }

    fn is_establishing(&self) -> bool {
        matches!(
            &*lock_unpoison(&self.state),
            OutboundState::AwaitAddr { .. }
                | OutboundState::AwaitRoute { .. }
                | OutboundState::AwaitEstablished { .. }
                | OutboundState::Transitioning
        )
    }

    fn is_disconnected(&self) -> bool {
        matches!(
            &*lock_unpoison(&self.state),
            OutboundState::DisconnectedAwaitingDelivery { .. }
                | OutboundState::Disconnected { .. }
                | OutboundState::FailedAwaitingDelivery { .. }
                | OutboundState::Failed { .. }
                | OutboundState::Closing { .. }
        )
    }

    fn mark_delivered(&self, request: &Arc<OutboundRequest>) {
        let mut state = lock_unpoison(&self.state);
        let replacement = match &*state {
            OutboundState::EstablishedAwaitingDelivery {
                request: route_request,
                connection,
            } if Arc::ptr_eq(route_request, request) => Some(OutboundState::Established {
                connection: connection.clone(),
            }),
            OutboundState::DisconnectedAwaitingDelivery {
                request: route_request,
                connection,
            } if Arc::ptr_eq(route_request, request) => Some(OutboundState::Disconnected {
                connection: connection.clone(),
            }),
            OutboundState::FailedAwaitingDelivery {
                request: route_request,
                connection,
            } if Arc::ptr_eq(route_request, request) => Some(OutboundState::Failed {
                connection: connection.clone(),
            }),
            _ => None,
        };
        if let Some(replacement) = replacement {
            *state = replacement;
        }
    }
}

#[derive(Clone)]
struct EstablishedConnectionRoute {
    token: ConnectionToken,
    state: Weak<ConnectionState>,
}

impl EstablishedConnectionRoute {
    fn new(connection: &Arc<ConnectionState>) -> Self {
        Self {
            token: connection.token,
            state: Arc::downgrade(connection),
        }
    }

    fn upgrade(&self) -> Option<Arc<ConnectionState>> {
        self.state.upgrade()
    }
}

enum OutboundState {
    AwaitAddr {
        cm_id: SharedCmId,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
    },
    AwaitRoute {
        cm_id: SharedCmId,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
    },
    AwaitEstablished {
        request: Arc<OutboundRequest>,
        connection: RdmaConnection,
    },
    EstablishedAwaitingDelivery {
        request: Arc<OutboundRequest>,
        connection: EstablishedConnectionRoute,
    },
    Established {
        connection: EstablishedConnectionRoute,
    },
    DisconnectedAwaitingDelivery {
        request: Arc<OutboundRequest>,
        connection: EstablishedConnectionRoute,
    },
    Disconnected {
        connection: EstablishedConnectionRoute,
    },
    FailedAwaitingDelivery {
        request: Arc<OutboundRequest>,
        connection: EstablishedConnectionRoute,
    },
    Failed {
        connection: EstablishedConnectionRoute,
    },
    Closing {
        connection: EstablishedConnectionRoute,
    },
    Transitioning,
}

impl OutboundState {
    fn references_connection(&self, token: ConnectionToken) -> bool {
        match self {
            Self::AwaitEstablished { connection, .. } => connection.state.token == token,
            Self::EstablishedAwaitingDelivery { connection, .. }
            | Self::Established { connection }
            | Self::DisconnectedAwaitingDelivery { connection, .. }
            | Self::Disconnected { connection }
            | Self::FailedAwaitingDelivery { connection, .. }
            | Self::Failed { connection }
            | Self::Closing { connection } => connection.token == token,
            Self::AwaitAddr { .. } | Self::AwaitRoute { .. } | Self::Transitioning => false,
        }
    }
}

struct OutboundRequest {
    address: SocketAddr,
    config: RdmaConnectionConfig,
    setup: Mutex<Option<Box<dyn PreEstablishSetup>>>,
    reservation: Mutex<Option<ConnectionReservation>>,
    result: Mutex<OutboundResult>,
    cancelled: AtomicBool,
    cancellation_enqueued: AtomicBool,
    failure_counted: AtomicBool,
    delivered: AtomicBool,
    route_token: AtomicU64,
    waker: AtomicWaker,
}

impl OutboundRequest {
    fn new(
        address: SocketAddr,
        config: RdmaConnectionConfig,
        setup: Box<dyn PreEstablishSetup>,
        reservation: ConnectionReservation,
    ) -> Self {
        Self {
            address,
            config,
            setup: Mutex::new(Some(setup)),
            reservation: Mutex::new(Some(reservation)),
            result: Mutex::new(OutboundResult::Pending),
            cancelled: AtomicBool::new(false),
            cancellation_enqueued: AtomicBool::new(false),
            failure_counted: AtomicBool::new(false),
            delivered: AtomicBool::new(false),
            route_token: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }

    fn take_setup(&self) -> Option<Box<dyn PreEstablishSetup>> {
        lock_unpoison(&self.setup).take()
    }

    fn take_reservation(&self) -> Option<ConnectionReservation> {
        lock_unpoison(&self.reservation).take()
    }

    fn complete(&self, result: Result<RdmaConnection>) {
        let mut current = lock_unpoison(&self.result);
        if matches!(&*current, OutboundResult::Pending) {
            *current = OutboundResult::Ready(result);
            drop(current);
            self.waker.wake();
        }
    }

    fn complete_failure(&self, shared: &EngineShared, error: Error) {
        self.record_failure(shared);
        self.complete(Err(error));
    }

    fn record_failure(&self, shared: &EngineShared) {
        if !self.failure_counted.swap(true, Ordering::AcqRel) {
            shared
                .diagnostic_counters
                .connections_failed
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn take_result(&self) -> Option<Result<RdmaConnection>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, OutboundResult::Taken) {
            OutboundResult::Ready(result) => Some(result),
            OutboundResult::Pending => {
                *current = OutboundResult::Pending;
                None
            }
            OutboundResult::Taken => None,
        }
    }

    fn cancel(&self, error: Error) {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let (replacement, undelivered) =
            match std::mem::replace(&mut *current, OutboundResult::Taken) {
                OutboundResult::Pending => (OutboundResult::Ready(Err(error)), None),
                OutboundResult::Ready(Ok(connection)) => {
                    (OutboundResult::Ready(Err(error)), Some(connection))
                }
                OutboundResult::Ready(Err(existing)) => {
                    (OutboundResult::Ready(Err(existing)), None)
                }
                OutboundResult::Taken => (OutboundResult::Taken, None),
            };
        *current = replacement;
        drop(current);
        drop(undelivered);
        self.waker.wake();
    }

    fn try_enqueue_cancellation(&self) -> bool {
        !self.cancellation_enqueued.swap(true, Ordering::AcqRel)
    }
}

enum OutboundResult {
    Pending,
    Ready(Result<RdmaConnection>),
    Taken,
}

struct ConnectWaiter {
    shared: Arc<EngineShared>,
    request: Arc<OutboundRequest>,
    finished: bool,
}

impl Future for ConnectWaiter {
    type Output = Result<RdmaConnection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.request.take_result() {
            if result.is_ok() {
                self.request.delivered.store(true, Ordering::Release);
                self.shared.cm.mark_request_delivered(&self.request);
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        self.request.waker.register(cx.waker());
        if let Some(result) = self.request.take_result() {
            if result.is_ok() {
                self.request.delivered.store(true, Ordering::Release);
                self.shared.cm.mark_request_delivered(&self.request);
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        Poll::Pending
    }
}

impl Drop for ConnectWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.request.cancel(Error::DriverShutdown);
        self.shared
            .cm
            .enqueue_cancellation(Arc::clone(&self.request));
        self.shared.work_signal.publish(CM_WORK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::engine::connection::{WorkRequestPoster, install_connection};
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};
    use std::task::{Context, Poll};

    #[test]
    fn route_tokens_round_trip_without_wrapping_or_pointer_encoding() {
        let token = CmRouteToken {
            slot: 0x00ff_ee11,
            generation: 0xaabb_ccdd,
        };
        assert_eq!(CmRouteToken::decode(token.encode()), token);
    }

    #[test]
    fn only_terminal_cm_classes_are_classified_as_failures() {
        for event in [
            CmEventType::AddrError,
            CmEventType::RouteError,
            CmEventType::ConnectError,
            CmEventType::Unreachable,
            CmEventType::Rejected,
            CmEventType::DeviceRemoval,
            CmEventType::AddrChange,
        ] {
            assert!(is_failure_event(event));
        }
        for event in [
            CmEventType::AddrResolved,
            CmEventType::RouteResolved,
            CmEventType::Established,
            CmEventType::Disconnected,
            CmEventType::TimewaitExit,
        ] {
            assert!(!is_failure_event(event));
        }
    }

    #[test]
    fn route_registry_rejects_wrong_id_stale_duplicate_and_unknown_events() {
        let cm = CmState::new(2).unwrap();
        let request = Arc::new(test_request());
        let (token, route) = cm
            .routes
            .allocate_with(|token| Arc::new(OutboundRoute::new(token, request)))
            .unwrap();
        route.set_identity(0x1000, 0x2000);
        lock_unpoison(&cm.context_routes).insert(0x2000, token);

        let exact = CmEventSnapshot {
            event_type: CmEventType::AddrResolved,
            status: 0,
            id: 0x1000,
            listen_id: 0,
            context_key: 0x2000,
        };
        assert!(cm.lookup_event_route(exact).is_ok());
        assert!(matches!(
            cm.lookup_event_route(CmEventSnapshot {
                id: 0x1001,
                ..exact
            }),
            Err(CmEventReject::WrongId)
        ));
        assert!(matches!(
            cm.lookup_event_route(CmEventSnapshot {
                context_key: 0x3000,
                ..exact
            }),
            Err(CmEventReject::Unknown)
        ));
        lock_unpoison(&cm.context_routes).insert(
            0x3001,
            CmRouteToken {
                slot: token.slot,
                generation: token.generation + 1,
            },
        );
        assert!(matches!(
            cm.lookup_event_route(CmEventSnapshot {
                context_key: 0x3001,
                ..exact
            }),
            Err(CmEventReject::Stale)
        ));

        cm.retire_route(&route, true);
        for event_type in [
            CmEventType::AddrResolved,
            CmEventType::Disconnected,
            CmEventType::TimewaitExit,
        ] {
            assert!(matches!(
                cm.lookup_event_route(CmEventSnapshot {
                    event_type,
                    ..exact
                }),
                Err(CmEventReject::Duplicate)
            ));
        }
    }

    #[test]
    fn duplicate_context_route_keeps_the_incumbent_mapping() {
        let cm = CmState::new(2).unwrap();
        let incumbent = CmRouteToken {
            slot: 0,
            generation: 1,
        };
        let duplicate = CmRouteToken {
            slot: 1,
            generation: 1,
        };

        assert!(cm.insert_context_route(0x2000, incumbent));
        assert!(!cm.insert_context_route(0x2000, duplicate));
        assert_eq!(
            lock_unpoison(&cm.context_routes).get(&0x2000).copied(),
            Some(incumbent)
        );
    }

    #[test]
    fn pre_establish_setup_completes_before_connect_and_failure_skips_connect() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(NoopPoster(7)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
        )
        .unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let summary = run_setup_before_connect(
            Box::new(RecordingSetup {
                order: Arc::clone(&order),
                result: Ok(SetupSummary { posted_wrs: 0 }),
            }),
            &engine.shared,
            &connection,
            || {
                lock_unpoison(&order).push("pre-connect");
                Ok(())
            },
            || {
                lock_unpoison(&order).push("connect");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(summary.posted_wrs, 0);
        assert_eq!(
            &*lock_unpoison(&order),
            &["setup", "pre-connect", "connect"]
        );

        lock_unpoison(&order).clear();
        let error = run_setup_before_connect(
            Box::new(RecordingSetup {
                order: Arc::clone(&order),
                result: Err(Error::InvalidConfig("setup failed".into())),
            }),
            &engine.shared,
            &connection,
            || {
                lock_unpoison(&order).push("pre-connect");
                Ok(())
            },
            || {
                lock_unpoison(&order).push("connect");
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
        assert_eq!(&*lock_unpoison(&order), &["setup"]);

        lock_unpoison(&order).clear();
        let error = run_setup_before_connect(
            Box::new(RecordingSetup {
                order: Arc::clone(&order),
                result: Ok(SetupSummary { posted_wrs: 1 }),
            }),
            &engine.shared,
            &connection,
            || {
                lock_unpoison(&order).push("pre-connect");
                Ok(())
            },
            || {
                lock_unpoison(&order).push("connect");
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
        assert_eq!(&*lock_unpoison(&order), &["setup"]);
        drop(driver);
    }

    #[test]
    fn delivery_replaces_the_frontend_with_weak_generational_route_state() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(NoopPoster(11)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
        )
        .unwrap();
        let request = Arc::new(test_request());
        let (_, route) = engine
            .shared
            .cm
            .routes
            .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
            .unwrap();
        route.set_state(OutboundState::EstablishedAwaitingDelivery {
            request: Arc::clone(&request),
            connection: EstablishedConnectionRoute::new(&connection.state),
        });
        request.complete(Ok(connection));
        let mut waiter = Box::pin(ConnectWaiter {
            shared: Arc::clone(&engine.shared),
            request,
            finished: false,
        });
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        let Poll::Ready(Ok(connection)) = waiter.as_mut().poll(&mut context) else {
            panic!("completed connection was not delivered");
        };
        drop(waiter);

        assert!(route.request().is_none());
        let state = lock_unpoison(&route.state);
        let OutboundState::Established { connection: routed } = &*state else {
            panic!("delivered route retained a pending-delivery state");
        };
        assert_eq!(routed.token, connection.state.token);
        assert!(routed.upgrade().is_some());
        drop(state);
        assert_eq!(
            Arc::strong_count(&engine.shared),
            3,
            "the route must not retain an RdmaConnection frontend"
        );

        engine.shared.cm.retire_route(&route, true);
        drop(connection);
        drop(engine);
        drop(driver);
    }

    #[test]
    fn shutdown_replaces_an_undelivered_success_and_enqueues_route_cleanup() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(NoopPoster(12)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
        )
        .unwrap();
        let request = Arc::new(test_request());
        let (_, route) = engine
            .shared
            .cm
            .routes
            .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
            .unwrap();
        route.set_state(OutboundState::EstablishedAwaitingDelivery {
            request: Arc::clone(&request),
            connection: EstablishedConnectionRoute::new(&connection.state),
        });
        request.complete(Ok(connection));

        engine.shared.cm.begin_shutdown(&EngineOutcome::Failure(
            super::super::EngineFailure::DriverShutdown,
        ));

        assert!(matches!(
            request.take_result(),
            Some(Err(Error::DriverShutdown))
        ));
        assert_eq!(lock_unpoison(&engine.shared.cm.cancellations).len(), 1);

        let processed = engine
            .shared
            .cm
            .service_software(&engine.shared, None, 1)
            .unwrap();
        assert_eq!(processed, 1);
        assert!(lock_unpoison(&engine.shared.cm.cancellations).is_empty());
        let _ = engine
            .shared
            .cm
            .service_software(&engine.shared, None, 1)
            .unwrap();
        drop(engine);
        drop(driver);
    }

    #[test]
    fn a_transitioning_route_resets_the_retirement_latch_and_retries() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let request = Arc::new(test_request());
        let (route_token, route) = engine
            .shared
            .cm
            .routes
            .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
            .unwrap();
        let reservation = reserve_connection(&engine.shared).unwrap();
        let connection = install_reserved_connection(
            &engine.shared,
            Arc::new(NoopPoster(13)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
            reservation,
            Some(route_token.encode()),
        )
        .unwrap();

        engine.shared.cm.enqueue_retirement(connection.state.token);
        let processed = engine
            .shared
            .cm
            .service_software(&engine.shared, None, 32)
            .unwrap();
        assert_eq!(
            processed, 1,
            "a requeued retirement may run only once per service pass"
        );
        assert_eq!(lock_unpoison(&engine.shared.cm.retirements).len(), 1);
        assert!(!connection.state.is_retired());

        route.set_state(OutboundState::Closing {
            connection: EstablishedConnectionRoute::new(&connection.state),
        });
        let processed = engine
            .shared
            .cm
            .service_software(&engine.shared, None, 32)
            .unwrap();
        assert_eq!(processed, 1);
        assert!(lock_unpoison(&engine.shared.cm.retirements).is_empty());
        assert!(connection.state.is_retired());
        assert!(matches!(
            engine.shared.connections.lookup(connection.state.token),
            Lookup::Duplicate
        ));

        drop(connection);
        drop(request);
        drop(engine);
        drop(driver);
    }

    #[test]
    fn cm_destroy_barrier_is_budgeted_across_service_passes() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let destroyed = Arc::new(AtomicBool::new(false));
        lock_unpoison(&engine.shared.cm.cm_destructions).push_back(PendingCmDestruction::Test {
            destroyed: Arc::clone(&destroyed),
        });
        let mut pending = VecDeque::from(["target", "peer"]);
        let mut routed = Vec::new();
        let mut probes = 0;

        for expected in ["target", "peer"] {
            let processed = engine
                .shared
                .cm
                .service_cm_destructions(&engine.shared, 1, || {
                    probes += 1;
                    let Some(event) = pending.pop_front() else {
                        return Ok(false);
                    };
                    routed.push(event);
                    Ok(true)
                })
                .unwrap();
            assert_eq!(processed, 1);
            assert_eq!(routed.last().copied(), Some(expected));
            assert!(!destroyed.load(Ordering::Acquire));
            assert_eq!(lock_unpoison(&engine.shared.cm.cm_destructions).len(), 1);
        }
        let processed = engine
            .shared
            .cm
            .service_cm_destructions(&engine.shared, 1, || {
                probes += 1;
                Ok(false)
            })
            .unwrap();
        assert_eq!(processed, 1);
        assert_eq!(probes, 3);
        assert_eq!(routed, ["target", "peer"]);
        assert!(pending.is_empty());
        assert!(destroyed.load(Ordering::Acquire));
        assert!(lock_unpoison(&engine.shared.cm.cm_destructions).is_empty());
        drop(engine);
        drop(driver);
    }

    #[test]
    fn request_failures_are_counted_once_but_shutdown_cancellation_is_not() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let failed = test_request();
        failed.complete_failure(&engine.shared, Error::InvalidConfig("first failure".into()));
        failed.complete_failure(
            &engine.shared,
            Error::InvalidConfig("duplicate failure".into()),
        );
        let cancelled = test_request();
        cancelled.cancel(Error::DriverShutdown);

        assert_eq!(
            engine
                .shared
                .diagnostic_counters
                .connections_failed
                .load(Ordering::Acquire),
            1
        );

        drop(engine);
        drop(driver);
    }

    struct RecordingSetup {
        order: Arc<Mutex<Vec<&'static str>>>,
        result: Result<SetupSummary>,
    }

    impl PreEstablishSetup for RecordingSetup {
        fn run(
            self: Box<Self>,
            _shared: &Arc<EngineShared>,
            _connection: &RdmaConnection,
        ) -> Result<SetupSummary> {
            lock_unpoison(&self.order).push("setup");
            self.result
        }
    }

    struct NoopPoster(u32);

    impl WorkRequestPoster for NoopPoster {
        fn qp_num(&self) -> u32 {
            self.0
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> BatchPostOutcome {
            BatchPostOutcome::AllAccepted
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> BatchPostOutcome {
            BatchPostOutcome::AllAccepted
        }

        fn to_error(&self) -> Result<()> {
            Ok(())
        }

        fn destroy_qp(&self) {}

        #[cfg(any(test, feature = "test-hooks"))]
        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn test_request() -> OutboundRequest {
        let pool = super::super::connection::ConnectionAdmissionPool::new(1);
        OutboundRequest::new(
            "127.0.0.1:1".parse().unwrap(),
            RdmaConnectionConfig::default(),
            Box::new(EmptyPreEstablishSetup),
            pool.try_acquire().unwrap(),
        )
    }
}
