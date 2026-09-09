//! Shared RDMA-CM routing and outbound connection state machines.
//!
//! The engine driver is the only consumer of the shared CM event channel.
//! Every outbound ID owns an opaque context allocation indexed to a
//! non-wrapping route token. Events are copied into an identity snapshot and
//! acknowledged before state ownership advances or any potentially blocking
//! librdmacm/verbs call runs.

mod event;
mod inbound;
mod outbound;
mod retirement;
mod shutdown;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

#[cfg(test)]
use super::super::SetupSummary;
use super::super::lifecycle::MemoizedTerminalResult;
use super::super::registry::{
    ConnectionToken, Lookup, PagedRegistry, RegistryToken, lock_unpoison,
};
use super::super::resources::EngineResources;
use super::super::{ConnectionSetup, RdmaConnection, RdmaConnectionConfig};
use super::SessionManager;
use super::connection::{
    ConnectionCmRoute, ConnectionReservation, ConnectionState, FailedConnectionInstallResources,
    SharedCmId, VerbsConnectionResources, WorkRequestPoster, install_reserved_connection,
    reserve_connection,
};
use super::listener::{
    AcceptRequest, InboundRejectReason, IncomingChild, KERNEL_LISTEN_BACKLOG_REQUEST,
    ListenRequest, ListenerAction, ListenerState, RdmaListener, empty_connection_setup,
    run_setup_before_establish,
};
#[cfg(test)]
use crate::cm::CmEventType;
use crate::cm::CmId;
use crate::v2::error::{Error, Result};
use crate::v2::qp::QpBuilder;

#[cfg(test)]
use event::CmDispatchRoute;
use event::{CmEventReject, CmEventSnapshot, EventDisposition, PendingCmEvent, is_failure_event};
#[cfg(test)]
use outbound::ConnectWaiter;
pub(in crate::v2::engine) use outbound::{OutboundRequest, connect, connect_with_setup};
pub(in crate::v2::engine) use shutdown::{CmShutdownClass, CmShutdownCursor, CmShutdownSnapshot};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextRoute {
    Outbound { token: CmRouteToken, raw_id: usize },
    Inbound { token: CmRouteToken, raw_id: usize },
    Listener { token: u64, raw_id: usize },
}

#[derive(Clone, Copy)]
pub(in crate::v2::engine) struct CmSoftwareSnapshot {
    remaining: [usize; 5],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum CmSoftwareClass {
    Cancellation,
    Retirement,
    OutboundStart,
    ListenStart,
    ListenerWork,
}

impl CmSoftwareClass {
    #[cfg(test)]
    pub(in crate::v2::engine) const ALL: [Self; 5] = [
        Self::Cancellation,
        Self::Retirement,
        Self::OutboundStart,
        Self::ListenStart,
        Self::ListenerWork,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Cancellation => 0,
            Self::Retirement => 1,
            Self::OutboundStart => 2,
            Self::ListenStart => 3,
            Self::ListenerWork => 4,
        }
    }
}

impl CmSoftwareSnapshot {
    pub(in crate::v2::engine) fn count(self, class: CmSoftwareClass) -> usize {
        self.remaining[class.index()]
    }
}

pub(in crate::v2::engine) struct CmState {
    routes: PagedRegistry<CmRouteToken, Arc<OutboundRoute>>,
    inbound_routes: PagedRegistry<CmRouteToken, Arc<InboundRoute>>,
    context_routes: Mutex<HashMap<usize, ContextRoute>>,
    pending: Mutex<VecDeque<Arc<OutboundRequest>>>,
    pending_listens: Mutex<VecDeque<Arc<ListenRequest>>>,
    cancellations: Mutex<VecDeque<Arc<OutboundRequest>>>,
    listener_work: Mutex<VecDeque<Arc<ListenerState>>>,
    listeners: Mutex<HashMap<u64, Arc<ListenerState>>>,
    listener_ids: Mutex<HashMap<usize, u64>>,
    next_listener_token: AtomicU64,
    retirements: Mutex<VecDeque<ConnectionToken>>,
    cm_destructions: Mutex<VecDeque<PendingCmDestruction>>,
    pending_event: Mutex<Option<PendingCmEvent>>,
    setup_rollback_quarantines: Mutex<Vec<RetainedSetupRollback>>,
    outbound_setup_active: AtomicBool,
    shutting_down: AtomicBool,
}

impl CmState {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            routes: PagedRegistry::new(capacity)?,
            inbound_routes: PagedRegistry::new(capacity)?,
            context_routes: Mutex::new(HashMap::new()),
            pending: Mutex::new(VecDeque::new()),
            pending_listens: Mutex::new(VecDeque::new()),
            cancellations: Mutex::new(VecDeque::new()),
            listener_work: Mutex::new(VecDeque::new()),
            listeners: Mutex::new(HashMap::new()),
            listener_ids: Mutex::new(HashMap::new()),
            next_listener_token: AtomicU64::new(1),
            retirements: Mutex::new(VecDeque::new()),
            cm_destructions: Mutex::new(VecDeque::new()),
            pending_event: Mutex::new(None),
            setup_rollback_quarantines: Mutex::new(Vec::new()),
            outbound_setup_active: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub(in crate::v2::engine) fn enqueue(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.pending).push_back(request);
    }

    pub(in crate::v2::engine) fn enqueue_listen(&self, request: Arc<ListenRequest>) {
        lock_unpoison(&self.pending_listens).push_back(request);
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_listen_addresses(&self) -> Vec<std::net::SocketAddr> {
        lock_unpoison(&self.pending_listens)
            .iter()
            .map(|request| request.address)
            .collect()
    }

    pub(in crate::v2::engine) fn enqueue_listener_work(&self, listener: &Arc<ListenerState>) {
        if listener.try_enqueue_work() {
            lock_unpoison(&self.listener_work).push_back(Arc::clone(listener));
        }
    }

    fn defer_cm_id(&self, cm_id: SharedCmId) {
        lock_unpoison(&self.cm_destructions).push_back(PendingCmDestruction::Route(cm_id));
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn defer_test_listener_destruction(
        &self,
        listener: Arc<ListenerState>,
        destroy_count: Arc<AtomicUsize>,
    ) {
        lock_unpoison(&self.cm_destructions).push_back(PendingCmDestruction::Test {
            destroy_count,
            target: TestCmDestruction::Listener {
                listener,
                destroy_error: None,
            },
        });
    }

    fn enqueue_cancellation(&self, request: Arc<OutboundRequest>) {
        if request.try_enqueue_cancellation() {
            lock_unpoison(&self.cancellations).push_back(request);
        }
    }

    pub(in crate::v2::engine) fn enqueue_retirement(&self, token: ConnectionToken) {
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

    pub(in crate::v2::engine) fn mark_accept_delivered(
        &self,
        listener: &Arc<ListenerState>,
        request: &Arc<AcceptRequest>,
    ) {
        let encoded = request.route_token();
        if encoded == 0 {
            return;
        }
        if let Lookup::Occupied(route) = self
            .inbound_routes
            .lookup_cloned(CmRouteToken::decode(encoded))
        {
            route.mark_delivered(request);
        }
        if listener.finish_selected_route(encoded) {
            self.enqueue_listener_work(listener);
        }
    }

    fn insert_context_route(&self, context_key: usize, route: ContextRoute) -> bool {
        match lock_unpoison(&self.context_routes).entry(context_key) {
            Entry::Vacant(entry) => {
                entry.insert(route);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    fn insert_listener_identity(
        &self,
        token: u64,
        raw_id: usize,
        listener: Arc<ListenerState>,
    ) -> bool {
        let mut listeners = lock_unpoison(&self.listeners);
        let mut listener_ids = lock_unpoison(&self.listener_ids);
        let Entry::Vacant(listener_entry) = listeners.entry(token) else {
            return false;
        };
        let Entry::Vacant(identity_entry) = listener_ids.entry(raw_id) else {
            return false;
        };
        listener_entry.insert(listener);
        identity_entry.insert(token);
        true
    }

    pub(in crate::v2::engine) fn has_software_work(&self) -> bool {
        let pending = !lock_unpoison(&self.pending).is_empty();
        let pending_listens = !lock_unpoison(&self.pending_listens).is_empty();
        let cancellations = !lock_unpoison(&self.cancellations).is_empty();
        let listener_work = !lock_unpoison(&self.listener_work).is_empty();
        let retirements = !lock_unpoison(&self.retirements).is_empty();
        let cm_destructions = !lock_unpoison(&self.cm_destructions).is_empty();
        pending
            || pending_listens
            || cancellations
            || listener_work
            || retirements
            || cm_destructions
    }

    pub(in crate::v2::engine) fn has_non_destruction_software_work(&self) -> bool {
        self.non_destruction_software_work_count() != 0
    }

    pub(in crate::v2::engine) fn non_destruction_software_work_count(&self) -> usize {
        lock_unpoison(&self.pending).len()
            + lock_unpoison(&self.pending_listens).len()
            + lock_unpoison(&self.cancellations).len()
            + lock_unpoison(&self.listener_work).len()
            + lock_unpoison(&self.retirements).len()
    }

    pub(in crate::v2::engine) fn has_destruction_work(&self) -> bool {
        self.destruction_work_count() != 0
    }

    pub(in crate::v2::engine) fn destruction_work_count(&self) -> usize {
        lock_unpoison(&self.cm_destructions).len()
    }

    fn take_pending_event(&self) -> Option<PendingCmEvent> {
        lock_unpoison(&self.pending_event).take()
    }

    pub(in crate::v2::engine) fn has_pending_event(&self) -> bool {
        lock_unpoison(&self.pending_event).is_some()
    }

    pub(in crate::v2::engine) fn defer_one_event(
        &self,
        resources: &EngineResources,
    ) -> Result<bool> {
        let mut pending = lock_unpoison(&self.pending_event);
        if pending.is_some() {
            return Ok(true);
        }
        let Some(event) = event::acquire_event(self, resources)? else {
            return Ok(false);
        };
        *pending = Some(event);
        Ok(true)
    }

    pub(in crate::v2::engine) fn service_software_class_into(
        &self,
        shared: &SessionManager,
        resources: Option<&EngineResources>,
        class: CmSoftwareClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        let mut processed = 0;
        while processed < budget && actions.remaining() >= 8 {
            match class {
                CmSoftwareClass::Cancellation => {
                    let request = { lock_unpoison(&self.cancellations).pop_front() };
                    if let Some(request) = request {
                        self.process_cancellation(shared, request, actions)?;
                        processed += 1;
                    } else {
                        break;
                    }
                }
                CmSoftwareClass::Retirement => {
                    let token = { lock_unpoison(&self.retirements).pop_front() };
                    if let Some(token) = token {
                        shared.retire_registered_connection_into(token, actions)?;
                        processed += 1;
                    } else {
                        break;
                    }
                }
                CmSoftwareClass::OutboundStart => {
                    let Some(resources) = resources else {
                        return Err(Error::InvalidConfig(
                            "CM pending work requires live engine resources".into(),
                        ));
                    };
                    let request = { lock_unpoison(&self.pending).pop_front() };
                    if let Some(request) = request {
                        if self.outbound_setup_active.swap(true, Ordering::AcqRel) {
                            lock_unpoison(&self.pending).push_front(request);
                            break;
                        }
                        if !self.start_outbound(resources, request, actions)? {
                            self.outbound_setup_active.store(false, Ordering::Release);
                        }
                        processed += 1;
                    } else {
                        break;
                    }
                }
                CmSoftwareClass::ListenStart => {
                    let resources = resources.ok_or_else(|| {
                        Error::InvalidConfig(
                            "listener creation requires live engine resources".into(),
                        )
                    })?;
                    let request = { lock_unpoison(&self.pending_listens).pop_front() };
                    if let Some(request) = request {
                        self.start_listener(shared, resources, request, actions)?;
                        processed += 1;
                    } else {
                        break;
                    }
                }
                CmSoftwareClass::ListenerWork => {
                    let resources = resources.ok_or_else(|| {
                        Error::InvalidConfig(
                            "listener progress requires live engine resources".into(),
                        )
                    })?;
                    let listener = { lock_unpoison(&self.listener_work).pop_front() };
                    if let Some(listener) = listener {
                        listener.begin_work();
                        self.service_listener(shared, resources, &listener, actions)?;
                        if listener.has_work() {
                            self.enqueue_listener_work(&listener);
                        }
                        processed += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        Ok(processed)
    }

    pub(in crate::v2::engine) fn software_snapshot(&self) -> CmSoftwareSnapshot {
        CmSoftwareSnapshot {
            remaining: [
                lock_unpoison(&self.cancellations).len(),
                lock_unpoison(&self.retirements).len(),
                lock_unpoison(&self.pending).len(),
                lock_unpoison(&self.pending_listens).len(),
                lock_unpoison(&self.listener_work).len(),
            ],
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_software(
        &self,
        shared: &SessionManager,
        resources: Option<&EngineResources>,
        budget: usize,
    ) -> Result<usize> {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let snapshot = self.software_snapshot();
        let mut processed = 0;
        for class in CmSoftwareClass::ALL {
            if processed == budget {
                break;
            }
            processed += self.service_software_class_into(
                shared,
                resources,
                class,
                snapshot.count(class).min(budget - processed),
                &mut actions,
            )?;
        }
        actions.publish();
        Ok(processed)
    }

    pub(in crate::v2::engine) fn try_process_event(
        &self,
        shared: &SessionManager,
        resources: &EngineResources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        event::try_process_event(self, shared, resources, actions)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn begin_shutdown(
        &self,
        shared: &SessionManager,
        outcome: &MemoizedTerminalResult,
    ) {
        shutdown::begin(self, shared, outcome);
    }

    pub(in crate::v2::engine) fn start_bounded_shutdown(&self) {
        shutdown::start(self);
    }

    pub(in crate::v2::engine) fn bounded_shutdown_snapshot(
        &self,
        terminalize_listeners: bool,
        cursor: &CmShutdownCursor,
        budget: usize,
    ) -> CmShutdownSnapshot {
        shutdown::snapshot(self, terminalize_listeners, cursor, budget)
    }

    pub(in crate::v2::engine) fn service_bounded_shutdown_class(
        &self,
        shared: &SessionManager,
        outcome: &MemoizedTerminalResult,
        terminalize_listeners: bool,
        cursor: &mut CmShutdownCursor,
        class: CmShutdownClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> usize {
        shutdown::service_class(
            self,
            shared,
            outcome,
            terminalize_listeners,
            cursor,
            class,
            budget,
            actions,
        )
    }

    pub(in crate::v2::engine) fn bounded_shutdown_complete(
        &self,
        cursor: &CmShutdownCursor,
    ) -> bool {
        shutdown::complete(self, cursor)
    }

    pub(in crate::v2::engine) fn pending_route_count(&self) -> usize {
        let establishing = self
            .routes
            .occupied_cloned()
            .into_iter()
            .filter(|route| route.is_establishing())
            .count();
        let pending = lock_unpoison(&self.pending).len();
        let pending_listens = lock_unpoison(&self.pending_listens).len();
        let cancellations = lock_unpoison(&self.cancellations).len();
        let listener_work = lock_unpoison(&self.listener_work).len();
        let retirements = lock_unpoison(&self.retirements).len();
        let cm_destructions = lock_unpoison(&self.cm_destructions).len();
        let inbound_routes = self.inbound_routes.live();
        let listeners = lock_unpoison(&self.listeners).len();
        establishing
            + pending
            + pending_listens
            + cancellations
            + listener_work
            + retirements
            + cm_destructions
            + inbound_routes
            + listeners
    }

    pub(in crate::v2::engine) fn retained_owner_count(&self) -> usize {
        let routes = self.routes.live();
        let inbound_routes = self.inbound_routes.live();
        let listeners = lock_unpoison(&self.listeners).len();
        let cm_destructions = lock_unpoison(&self.cm_destructions).len();
        let routed_owners = routes + inbound_routes + listeners + cm_destructions;
        // Every retained setup rollback still owns its live CM route. The
        // maximum counts those overlapping owners once while flooring the
        // result if a future unregistered rollback ever loses route coverage.
        let setup_rollback_quarantines = lock_unpoison(&self.setup_rollback_quarantines).len();
        routed_owners.max(setup_rollback_quarantines)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn connection_route_is_live(
        &self,
        route: ConnectionCmRoute,
        connection: ConnectionToken,
    ) -> bool {
        let token = match route {
            ConnectionCmRoute::Outbound(encoded) | ConnectionCmRoute::Inbound(encoded) => {
                CmRouteToken::decode(encoded)
            }
        };
        match route {
            ConnectionCmRoute::Outbound(_) => {
                let Lookup::Occupied(route) = self.routes.lookup_cloned(token) else {
                    return false;
                };
                lock_unpoison(&route.state).references_connection(connection)
            }
            ConnectionCmRoute::Inbound(_) => {
                let Lookup::Occupied(route) = self.inbound_routes.lookup_cloned(token) else {
                    return false;
                };
                lock_unpoison(&route.state).references_connection(connection)
            }
        }
    }

    pub(in crate::v2::engine) fn service_cm_destructions_into(
        &self,
        shared: &SessionManager,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
        defer_one_event: impl FnMut() -> Result<bool>,
    ) -> Result<usize> {
        retirement::service_cm_destructions(self, shared, budget, actions, defer_one_event)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_cm_destructions(
        &self,
        shared: &SessionManager,
        budget: usize,
        mut try_process_event: impl FnMut() -> Result<bool>,
    ) -> Result<usize> {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let result =
            self.service_cm_destructions_into(shared, budget, &mut actions, || try_process_event());
        actions.publish();
        result
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn terminalize(&self, outcome: &MemoizedTerminalResult) {
        shutdown::terminalize(self, outcome);
    }

    pub(in crate::v2::engine) fn terminalize_into(
        &self,
        outcome: &MemoizedTerminalResult,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        shutdown::terminalize_into(self, outcome, actions);
    }

    fn start_listener(
        &self,
        shared: &SessionManager,
        resources: &EngineResources,
        request: Arc<ListenRequest>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        inbound::start_listener(self, shared, resources, request, actions)
    }

    fn service_listener(
        &self,
        shared: &SessionManager,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        inbound::service_listener(self, shared, resources, listener, actions)
    }

    fn handle_connect_request(
        &self,
        shared: &SessionManager,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        inbound::handle_connect_request(self, shared, resources, listener, snapshot)
    }

    fn handle_listener_event(
        &self,
        shared: &SessionManager,
        listener: &Arc<ListenerState>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        inbound::handle_listener_event(self, shared, listener, snapshot)
    }

    fn reject_raw_child(
        &self,
        resources: &EngineResources,
        raw_id: usize,
        reason: InboundRejectReason,
    ) -> Result<()> {
        inbound::reject_raw_child(self, resources, raw_id, reason)
    }

    fn start_outbound(
        &self,
        resources: &EngineResources,
        request: Arc<OutboundRequest>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        outbound::start(self, resources, request, actions)
    }

    fn process_cancellation(
        &self,
        shared: &SessionManager,
        request: Arc<OutboundRequest>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        outbound::process_cancellation(self, shared, request, actions)
    }

    fn release_failed_install(
        &self,
        shared: &SessionManager,
        resources: FailedConnectionInstallResources,
    ) -> Result<()> {
        retirement::release_failed_install(shared, resources)
    }

    fn retain_failed_install(
        &self,
        shared: &SessionManager,
        resources: FailedConnectionInstallResources,
        destroy_error: &Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Option<EstablishedConnectionRoute> {
        retirement::retain_failed_install(self, shared, resources, destroy_error, actions)
    }

    fn record_setup_rollback_quarantine(destroy_error: &Error) {
        retirement::record_setup_rollback_quarantine(destroy_error);
    }

    #[cfg(test)]
    fn lookup_dispatch_route(
        &self,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<CmDispatchRoute, CmEventReject> {
        event::lookup_dispatch_route(self, snapshot)
    }

    #[cfg(test)]
    fn lookup_event_route(
        &self,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<Arc<OutboundRoute>, CmEventReject> {
        event::lookup_event_route(self, snapshot)
    }

    fn handle_event(
        &self,
        shared: &SessionManager,
        resources: &EngineResources,
        route: &Arc<OutboundRoute>,
        snapshot: CmEventSnapshot,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<EventDisposition> {
        outbound::handle_event(self, shared, resources, route, snapshot, actions)
    }

    fn handle_inbound_event(
        &self,
        shared: &SessionManager,
        route: &Arc<InboundRoute>,
        snapshot: CmEventSnapshot,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<EventDisposition> {
        inbound::handle_event(self, shared, route, snapshot, actions)
    }

    #[cfg(test)]
    fn handle_inbound_disconnected(
        &self,
        shared: &SessionManager,
        route: &Arc<InboundRoute>,
    ) -> Result<EventDisposition> {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let result = inbound::handle_disconnected(self, shared, route, &mut actions);
        actions.publish();
        result
    }

    fn retire_route(&self, route: &Arc<OutboundRoute>, completed: bool) {
        self.routes.release(route.token, completed);
    }

    fn retire_outbound_route_for_retirement(
        &self,
        encoded: u64,
        connection: &Arc<ConnectionState>,
    ) -> Result<RouteRetirement> {
        let token = CmRouteToken::decode(encoded);
        let route = match self.routes.lookup_cloned(token) {
            Lookup::Occupied(route) => route,
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                return Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                });
            }
        };
        let route_state =
            route.take_state_if(|route_state| route_state.references_connection(connection.token));
        match route_state {
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
                Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                })
            }
            Some(route_state) => {
                route.set_state(route_state);
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

    fn retire_inbound_route_for_retirement(
        &self,
        encoded: u64,
        connection: &Arc<ConnectionState>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<RouteRetirement> {
        let token = CmRouteToken::decode(encoded);
        let route = match self.inbound_routes.lookup_cloned(token) {
            Lookup::Occupied(route) => route,
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                return Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                });
            }
        };
        let route_state =
            route.take_state_if(|route_state| route_state.references_connection(connection.token));
        match route_state {
            Some(InboundState::EstablishedAwaitingDelivery { request, .. }) => {
                let delivered = request.fail_undelivered_into(Error::DriverShutdown, actions);
                if delivered
                    && let Some(listener) = route.listener.upgrade()
                    && listener.finish_selected_route(encoded)
                {
                    self.enqueue_listener_work(&listener);
                }
                self.inbound_routes.release(token, true);
                Ok(RouteRetirement::Complete {
                    completion: (!delivered).then(|| InboundRetirementCompletion {
                        listener: route.listener.clone(),
                        route: encoded,
                        request: None,
                        result: None,
                        selected: true,
                    }),
                    reject: None,
                })
            }
            Some(InboundState::Established { .. }) => {
                self.inbound_routes.release(token, true);
                Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                })
            }
            Some(InboundState::Closing {
                request,
                completion,
                selected,
                reject,
                ..
            }) => {
                self.inbound_routes.release(token, true);
                Ok(RouteRetirement::Complete {
                    completion: Some(InboundRetirementCompletion {
                        listener: route.listener.clone(),
                        route: encoded,
                        request,
                        result: completion,
                        selected,
                    }),
                    reject,
                })
            }
            Some(route_state) => {
                route.set_state(route_state);
                Err(Error::InvalidConfig(
                    "inbound connection route was not established during retirement".into(),
                ))
            }
            None if matches!(&*lock_unpoison(&route.state), InboundState::Transitioning) => {
                Ok(RouteRetirement::Retry)
            }
            None => Err(Error::InvalidConfig(
                "inbound connection route generation did not match retirement".into(),
            )),
        }
    }

    fn remove_owned_context_route(&self, cm_id: Option<&SharedCmId>) {
        let Some(cm_id) = cm_id else {
            return;
        };
        let Some(route_token) = cm_id.context_token() else {
            return;
        };
        let context_key = cm_id.context_key();
        let raw_id = cm_id.as_raw() as usize;
        self.remove_context_route_if_owned(context_key, raw_id, Some(route_token));
    }

    fn remove_context_route_if_owned(
        &self,
        context_key: usize,
        raw_id: usize,
        route_token: Option<u64>,
    ) -> bool {
        let Some(route_token) = route_token else {
            return false;
        };
        let mut routes = lock_unpoison(&self.context_routes);
        let owned = match routes.get(&context_key).copied() {
            Some(ContextRoute::Outbound {
                token,
                raw_id: owner,
            })
            | Some(ContextRoute::Inbound {
                token,
                raw_id: owner,
            }) => token.encode() == route_token && owner == raw_id,
            Some(ContextRoute::Listener {
                token,
                raw_id: owner,
            }) => token == route_token && owner == raw_id,
            None => false,
        };
        if owned {
            routes.remove(&context_key);
        }
        owned
    }
}

impl SessionManager {
    #[cfg(test)]
    pub(in crate::v2::engine) fn terminalize_cm(&self, outcome: &MemoizedTerminalResult) {
        self.cm.terminalize(outcome);
    }

    pub(in crate::v2::engine) fn terminalize_cm_into(
        &self,
        outcome: &MemoizedTerminalResult,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.cm.terminalize_into(outcome, actions);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn pending_cm_route_count(&self) -> usize {
        self.cm.pending_route_count()
    }

    pub(in crate::v2::engine) fn retained_cm_owner_count(&self) -> usize {
        self.cm.retained_owner_count()
    }

    pub(in crate::v2::engine) fn has_cm_work(&self) -> bool {
        self.cm.has_software_work()
    }

    pub(in crate::v2::engine) fn service_cm_software_class(
        &self,
        resources: Option<&EngineResources>,
        class: CmSoftwareClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        self.cm
            .service_software_class_into(self, resources, class, budget, actions)
    }

    pub(in crate::v2::engine) fn cm_software_snapshot(&self) -> CmSoftwareSnapshot {
        self.cm.software_snapshot()
    }

    pub(in crate::v2::engine) fn try_process_cm_event(
        &self,
        resources: &EngineResources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        self.cm.try_process_event(self, resources, actions)
    }

    pub(in crate::v2::engine) fn has_pending_cm_event(&self) -> bool {
        self.cm.has_pending_event()
    }

    pub(in crate::v2::engine) fn service_deferred_cm_destructions(
        &self,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
        defer_one_event: impl FnMut() -> Result<bool>,
    ) -> Result<usize> {
        self.cm
            .service_cm_destructions_into(self, budget, actions, defer_one_event)
    }

    pub(in crate::v2::engine) fn retire_registered_connection_into(
        &self,
        token: ConnectionToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        retirement::retire_registered_connection_into(self, token, actions)
    }
}

fn build_qp(
    resources: &EngineResources,
    cm_id: &CmId,
    config: &RdmaConnectionConfig,
) -> Result<crate::v2::Qp> {
    QpBuilder::new(&resources.pd, &resources.cq, &resources.cq)
        .max_send_wr(
            u32::try_from(config.max_send_wr)
                .map_err(|_| Error::InvalidConfig("maximum send WRs do not fit u32".into()))?,
        )
        .max_recv_wr(
            u32::try_from(config.max_recv_wr)
                .map_err(|_| Error::InvalidConfig("maximum receive WRs do not fit u32".into()))?,
        )
        .max_send_sge(
            u32::try_from(config.max_send_sge)
                .map_err(|_| Error::InvalidConfig("maximum send SGEs do not fit u32".into()))?,
        )
        .max_recv_sge(
            u32::try_from(config.max_recv_sge)
                .map_err(|_| Error::InvalidConfig("maximum receive SGEs do not fit u32".into()))?,
        )
        .sq_sig_all(true)
        .build_with_cm(cm_id)
}

fn contextual_cm_error(context: impl Into<String>, error: Error) -> Error {
    let context = context.into();
    match error {
        Error::Verbs(source) => Error::Verbs(std::io::Error::new(
            source.kind(),
            format!("{context}: {source}"),
        )),
        other => Error::InvalidConfig(format!("{context}: {other}")),
    }
}

fn error_detail(error: &Error) -> String {
    match error {
        Error::Verbs(source) | Error::PostFailed(source) => source.to_string(),
        Error::InvalidConfig(message) | Error::ProtocolViolation(message) => message.clone(),
        other => other.to_string(),
    }
}

fn connection_destruction_error(destroy_result: Result<()>, finalize_result: Result<()>) -> Error {
    match (destroy_result, finalize_result) {
        (Err(destroy), Ok(())) => destroy,
        (Ok(()), Err(finalize)) => contextual_cm_error(
            "finalize connection retirement after CM destruction",
            finalize,
        ),
        (Err(destroy), Err(finalize)) => Error::Verbs(std::io::Error::other(format!(
            "{}; additionally failed to finalize connection retirement: {}",
            error_detail(&destroy),
            error_detail(&finalize)
        ))),
        (Ok(()), Ok(())) => {
            unreachable!("connection destruction error requires at least one failure")
        }
    }
}

#[cfg(test)]
fn injected_cm_result(error: Option<String>) -> Result<()> {
    match error {
        Some(error) => Err(Error::Verbs(std::io::Error::other(error))),
        None => Ok(()),
    }
}

enum RouteRetirement {
    Complete {
        completion: Option<InboundRetirementCompletion>,
        reject: Option<InboundRejectReason>,
    },
    Retry,
}

enum PendingCmDestruction {
    Route(SharedCmId),
    Connection {
        cm_id: SharedCmId,
        connection: Arc<ConnectionState>,
        completion: Option<InboundRetirementCompletion>,
    },
    Listener {
        cm_id: SharedCmId,
        listener: Arc<ListenerState>,
    },
    #[cfg(test)]
    Test {
        destroy_count: Arc<AtomicUsize>,
        target: TestCmDestruction,
    },
}

struct RetainedSetupRollback {
    _poster: Arc<dyn WorkRequestPoster>,
    _reservation: ConnectionReservation,
}

#[cfg(test)]
enum TestCmDestruction {
    Listener {
        listener: Arc<ListenerState>,
        destroy_error: Option<String>,
    },
    Connection {
        connection: Arc<ConnectionState>,
        completion: Option<InboundRetirementCompletion>,
        destroy_error: Option<String>,
        finalize_error: Option<String>,
    },
}

impl PendingCmDestruction {
    fn cm_id(&self) -> Option<&SharedCmId> {
        match self {
            Self::Route(cm_id) | Self::Connection { cm_id, .. } | Self::Listener { cm_id, .. } => {
                Some(cm_id)
            }
            #[cfg(test)]
            Self::Test { .. } => None,
        }
    }

    fn listener(&self) -> Option<&Arc<ListenerState>> {
        match self {
            Self::Listener { listener, .. } => Some(listener),
            #[cfg(test)]
            Self::Test {
                target: TestCmDestruction::Listener { listener, .. },
                ..
            } => Some(listener),
            #[cfg(test)]
            Self::Test {
                target: TestCmDestruction::Connection { .. },
                ..
            } => None,
            Self::Route(_) | Self::Connection { .. } => None,
        }
    }
}

struct InboundRoute {
    token: CmRouteToken,
    raw_id: AtomicUsize,
    context_key: AtomicUsize,
    listener: Weak<ListenerState>,
    state: Mutex<InboundState>,
}

impl InboundRoute {
    fn new(token: CmRouteToken, listener: Weak<ListenerState>) -> Self {
        Self {
            token,
            raw_id: AtomicUsize::new(0),
            context_key: AtomicUsize::new(0),
            listener,
            state: Mutex::new(InboundState::Transitioning),
        }
    }

    fn set_identity(&self, raw_id: usize, context_key: usize) {
        self.raw_id.store(raw_id, Ordering::Release);
        self.context_key.store(context_key, Ordering::Release);
    }

    fn set_state(&self, state: InboundState) {
        *lock_unpoison(&self.state) = state;
    }

    fn take_state_if(&self, predicate: impl FnOnce(&InboundState) -> bool) -> Option<InboundState> {
        let mut state = lock_unpoison(&self.state);
        if !predicate(&state) {
            return None;
        }
        Some(std::mem::replace(&mut *state, InboundState::Transitioning))
    }

    fn mark_delivered(&self, request: &Arc<AcceptRequest>) -> bool {
        let mut state = lock_unpoison(&self.state);
        let replacement = match &*state {
            InboundState::EstablishedAwaitingDelivery {
                request: current,
                connection,
            } if Arc::ptr_eq(current, request) => Some(InboundState::Established {
                connection: connection.clone(),
            }),
            _ => None,
        };
        if let Some(replacement) = replacement {
            *state = replacement;
            true
        } else {
            false
        }
    }
}

enum InboundState {
    AwaitEstablished {
        request: Arc<AcceptRequest>,
        connection: RdmaConnection,
    },
    EstablishedAwaitingDelivery {
        request: Arc<AcceptRequest>,
        connection: EstablishedConnectionRoute,
    },
    Established {
        connection: EstablishedConnectionRoute,
    },
    Closing {
        connection: EstablishedConnectionRoute,
        request: Option<Arc<AcceptRequest>>,
        completion: Option<Error>,
        selected: bool,
        reject: Option<InboundRejectReason>,
    },
    Quarantined {
        connection: Option<EstablishedConnectionRoute>,
    },
    Transitioning,
}

impl InboundState {
    fn references_connection(&self, token: ConnectionToken) -> bool {
        match self {
            Self::AwaitEstablished { connection, .. } => connection.session_token() == token,
            Self::EstablishedAwaitingDelivery { connection, .. }
            | Self::Established { connection }
            | Self::Closing { connection, .. } => connection.token == token,
            Self::Quarantined { connection } => connection
                .as_ref()
                .is_some_and(|connection| connection.token == token),
            Self::Transitioning => false,
        }
    }
}

struct InboundRetirementCompletion {
    listener: Weak<ListenerState>,
    route: u64,
    request: Option<Arc<AcceptRequest>>,
    result: Option<Error>,
    selected: bool,
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
            | OutboundState::Quarantined { .. }
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
                | OutboundState::Quarantined { .. }
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
    Quarantined {
        connection: Option<EstablishedConnectionRoute>,
    },
    Transitioning,
}

impl OutboundState {
    fn references_connection(&self, token: ConnectionToken) -> bool {
        match self {
            Self::AwaitEstablished { connection, .. } => connection.session_token() == token,
            Self::EstablishedAwaitingDelivery { connection, .. }
            | Self::Established { connection }
            | Self::DisconnectedAwaitingDelivery { connection, .. }
            | Self::Disconnected { connection }
            | Self::FailedAwaitingDelivery { connection, .. }
            | Self::Failed { connection }
            | Self::Closing { connection } => connection.token == token,
            Self::Quarantined { connection } => connection
                .as_ref()
                .is_some_and(|connection| connection.token == token),
            Self::AwaitAddr { .. } | Self::AwaitRoute { .. } | Self::Transitioning => false,
        }
    }
}

#[cfg(test)]
mod tests;
