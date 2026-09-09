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
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::super::lifecycle::MemoizedTerminalResult;
use super::super::registry::{ConnectionToken, Lookup, lock_unpoison};
use super::super::resources::EngineResources;
use super::super::{ConnectionSetup, RdmaConnection, RdmaConnectionConfig};
use super::SessionManager;
use super::connection::{
    ConnectionCmRoute, ConnectionReservation, FailedConnectionInstallResources, SharedCmId,
    VerbsConnectionResources, install_reserved_connection, reserve_connection,
};
use super::listener::{
    AcceptRequest, InboundRejectReason, IncomingChild, KERNEL_LISTEN_BACKLOG_REQUEST,
    ListenRequest, ListenerAction, ListenerState, RdmaListener, empty_connection_setup,
    run_setup_before_establish,
};
use super::registry::ConnectionRegistry;
use crate::cm::CmId;
use crate::v2::error::{Error, Result};
use crate::v2::qp::QpBuilder;

#[cfg(test)]
use event::CmDispatchRoute;
use event::{CmEventReject, CmEventSnapshot, EventDisposition, PendingCmEvent, is_failure_event};
pub(in crate::v2::engine) use outbound::{OutboundRequest, connect, connect_with_setup};
pub(in crate::v2::engine) use shutdown::{CmShutdownClass, CmShutdownCursor, CmShutdownSnapshot};

type CmRouteToken = ConnectionToken;

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
    context_routes: Mutex<HashMap<usize, ContextRoute>>,
    pending_listens: Mutex<VecDeque<Arc<ListenRequest>>>,
    listener_work: Mutex<VecDeque<Arc<ListenerState>>>,
    listeners: Mutex<HashMap<u64, Arc<ListenerState>>>,
    listener_ids: Mutex<HashMap<usize, u64>>,
    next_listener_token: AtomicU64,
    cm_destructions: Mutex<VecDeque<PendingCmDestruction>>,
    pending_event: Mutex<Option<PendingCmEvent>>,
    shutting_down: AtomicBool,
}

impl CmState {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
        let _ = capacity;
        Ok(Self {
            context_routes: Mutex::new(HashMap::new()),
            pending_listens: Mutex::new(VecDeque::new()),
            listener_work: Mutex::new(VecDeque::new()),
            listeners: Mutex::new(HashMap::new()),
            listener_ids: Mutex::new(HashMap::new()),
            next_listener_token: AtomicU64::new(1),
            cm_destructions: Mutex::new(VecDeque::new()),
            pending_event: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        })
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

    pub(in crate::v2::engine) fn mark_accept_delivered(
        &self,
        listener: &Arc<ListenerState>,
        request: &Arc<AcceptRequest>,
    ) {
        let encoded = request.route_token();
        if encoded == 0 {
            return;
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

    pub(in crate::v2::engine) fn has_software_work(
        &self,
        connections: &ConnectionRegistry,
    ) -> bool {
        let pending = connections.pending_outbound_count() != 0;
        let pending_listens = !lock_unpoison(&self.pending_listens).is_empty();
        let cancellations = connections.cancellation_count() != 0;
        let listener_work = !lock_unpoison(&self.listener_work).is_empty();
        let retirements = connections.retirement_count() != 0;
        let cm_destructions = !lock_unpoison(&self.cm_destructions).is_empty();
        pending
            || pending_listens
            || cancellations
            || listener_work
            || retirements
            || cm_destructions
    }

    pub(in crate::v2::engine) fn has_non_destruction_software_work(
        &self,
        connections: &ConnectionRegistry,
    ) -> bool {
        self.non_destruction_software_work_count(connections) != 0
    }

    pub(in crate::v2::engine) fn non_destruction_software_work_count(
        &self,
        connections: &ConnectionRegistry,
    ) -> usize {
        connections.pending_outbound_count()
            + lock_unpoison(&self.pending_listens).len()
            + connections.cancellation_count()
            + lock_unpoison(&self.listener_work).len()
            + connections.retirement_count()
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
        connections: &ConnectionRegistry,
        resources: &EngineResources,
    ) -> Result<bool> {
        let mut pending = lock_unpoison(&self.pending_event);
        if pending.is_some() {
            return Ok(true);
        }
        let Some(event) = event::acquire_event(self, connections, resources)? else {
            return Ok(false);
        };
        *pending = Some(event);
        Ok(true)
    }

    pub(in crate::v2::engine) fn service_software_class_into(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineResources>,
        class: CmSoftwareClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        let mut processed = 0;
        while processed < budget && actions.remaining() >= 8 {
            match class {
                CmSoftwareClass::Cancellation => {
                    let request = connections.pop_cancellation();
                    if let Some(request) = request {
                        self.process_cancellation(connections, shared, io_core, request, actions)?;
                        processed += 1;
                    } else {
                        break;
                    }
                }
                CmSoftwareClass::Retirement => {
                    let token = connections.pop_retirement();
                    if let Some(token) = token {
                        retirement::retire_registered_connection_into(
                            self,
                            connections,
                            shared,
                            io_core,
                            token,
                            actions,
                        )?;
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
                    let pending = connections.pop_outbound();
                    if let Some((request, reservation)) = pending {
                        if !connections.try_begin_outbound_setup() {
                            connections.push_front_outbound(request, reservation);
                            break;
                        }
                        if !self.start_outbound(
                            connections,
                            resources,
                            request,
                            reservation,
                            actions,
                        )? {
                            connections.finish_outbound_setup();
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
                        self.service_listener(
                            connections,
                            shared,
                            io_core,
                            resources,
                            &listener,
                            actions,
                        )?;
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

    pub(in crate::v2::engine) fn software_snapshot(
        &self,
        connections: &ConnectionRegistry,
    ) -> CmSoftwareSnapshot {
        CmSoftwareSnapshot {
            remaining: [
                connections.cancellation_count(),
                connections.retirement_count(),
                connections.pending_outbound_count(),
                lock_unpoison(&self.pending_listens).len(),
                lock_unpoison(&self.listener_work).len(),
            ],
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_software(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineResources>,
        budget: usize,
    ) -> Result<usize> {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let snapshot = self.software_snapshot(connections);
        let mut processed = 0;
        for class in CmSoftwareClass::ALL {
            if processed == budget {
                break;
            }
            processed += self.service_software_class_into(
                connections,
                shared,
                io_core,
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
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineResources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        event::try_process_event(self, connections, shared, io_core, resources, actions)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn begin_shutdown(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        outcome: &MemoizedTerminalResult,
    ) {
        shutdown::begin(self, connections, shared, outcome);
    }

    pub(in crate::v2::engine) fn start_bounded_shutdown(&self) {
        shutdown::start(self);
    }

    pub(in crate::v2::engine) fn bounded_shutdown_snapshot(
        &self,
        connections: &ConnectionRegistry,
        terminalize_listeners: bool,
        cursor: &CmShutdownCursor,
        budget: usize,
    ) -> CmShutdownSnapshot {
        shutdown::snapshot(self, connections, terminalize_listeners, cursor, budget)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "bounded shutdown keeps ownership inputs explicit"
    )]
    pub(in crate::v2::engine) fn service_bounded_shutdown_class(
        &self,
        connections: &mut ConnectionRegistry,
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
            connections,
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
        connections: &ConnectionRegistry,
        cursor: &CmShutdownCursor,
    ) -> bool {
        shutdown::complete(self, connections, cursor)
    }

    pub(in crate::v2::engine) fn retained_owner_count(
        &self,
        connections: &ConnectionRegistry,
    ) -> usize {
        let listeners = lock_unpoison(&self.listeners).len();
        let cm_destructions = lock_unpoison(&self.cm_destructions).len();
        connections.live() + listeners + cm_destructions
    }

    pub(in crate::v2::engine) fn retained_adapter_owner_count(&self) -> usize {
        let listeners = lock_unpoison(&self.listeners).len();
        let cm_destructions = lock_unpoison(&self.cm_destructions).len();
        listeners + cm_destructions
    }

    pub(in crate::v2::engine) fn pending_adapter_route_count(&self) -> usize {
        0 + lock_unpoison(&self.pending_listens).len()
            + lock_unpoison(&self.listener_work).len()
            + lock_unpoison(&self.cm_destructions).len()
            + lock_unpoison(&self.listeners).len()
    }

    pub(in crate::v2::engine) fn service_cm_destructions_into(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut crate::v2::engine::io_core::IoState,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
        defer_one_event: impl FnMut(&ConnectionRegistry) -> Result<bool>,
    ) -> Result<usize> {
        retirement::service_cm_destructions(
            self,
            connections,
            io_core,
            budget,
            actions,
            defer_one_event,
        )
    }

    pub(in crate::v2::engine) fn terminalize_into(
        &self,
        connections: &mut ConnectionRegistry,
        outcome: &MemoizedTerminalResult,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        shutdown::terminalize_into(self, connections, outcome, actions);
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
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        inbound::service_listener(
            self,
            connections,
            shared,
            io_core,
            resources,
            listener,
            actions,
        )
    }

    fn handle_connect_request(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        inbound::handle_connect_request(self, connections, shared, resources, listener, snapshot)
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
        connections: &mut ConnectionRegistry,
        resources: &EngineResources,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        outbound::start(self, connections, resources, request, reservation, actions)
    }

    fn process_cancellation(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        request: Arc<OutboundRequest>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        outbound::process_cancellation(self, connections, shared, io_core, request, actions)
    }

    fn release_failed_install(
        &self,
        connections: &mut ConnectionRegistry,
        resources: FailedConnectionInstallResources,
    ) -> Result<()> {
        retirement::release_failed_install(connections, resources)
    }

    fn retain_failed_install(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        token: ConnectionToken,
        resources: FailedConnectionInstallResources,
        destroy_error: &Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Option<EstablishedConnectionRoute> {
        retirement::retain_failed_install(
            self,
            connections,
            shared,
            token,
            resources,
            destroy_error,
            actions,
        )
    }

    fn record_setup_rollback_quarantine(destroy_error: &Error) {
        retirement::record_setup_rollback_quarantine(destroy_error);
    }

    #[cfg(test)]
    fn lookup_dispatch_route(
        &self,
        connections: &ConnectionRegistry,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<CmDispatchRoute, CmEventReject> {
        event::lookup_dispatch_route(self, connections, snapshot)
    }

    #[cfg(test)]
    fn lookup_event_route(
        &self,
        connections: &ConnectionRegistry,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<super::registry::ConnectionRouteIdentity, CmEventReject> {
        event::lookup_event_route(self, connections, snapshot)
    }

    fn handle_event(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineResources,
        token: ConnectionToken,
        snapshot: CmEventSnapshot,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<EventDisposition> {
        outbound::handle_event(
            self,
            connections,
            shared,
            io_core,
            resources,
            token,
            snapshot,
            actions,
        )
    }

    fn handle_inbound_event(
        &self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        token: ConnectionToken,
        snapshot: CmEventSnapshot,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<EventDisposition> {
        inbound::handle_event(self, connections, shared, io_core, token, snapshot, actions)
    }

    fn retire_route(
        &self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        completed: bool,
    ) {
        connections.release_route(token, completed);
    }

    fn retire_outbound_route_for_retirement(
        &self,
        connections: &mut ConnectionRegistry,
        encoded: u64,
        connection: ConnectionToken,
    ) -> Result<RouteRetirement> {
        let token = CmRouteToken::decode(encoded);
        match connections.lookup_outbound(token) {
            Lookup::Occupied(_) => {}
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                return Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                });
            }
        }
        let route_state = connections.take_outbound_state_if(token, |route_state| {
            route_state.references_connection(connection)
        });
        match route_state {
            Some(
                OutboundState::EstablishedAwaitingDelivery { .. }
                | OutboundState::DisconnectedAwaitingDelivery { .. }
                | OutboundState::Disconnected { .. }
                | OutboundState::FailedAwaitingDelivery { .. }
                | OutboundState::Failed { .. }
                | OutboundState::Closing { .. },
            ) => {
                self.retire_route(connections, token, true);
                Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                })
            }
            Some(route_state) => {
                connections.set_outbound_state(token, route_state);
                Err(Error::InvalidConfig(
                    "connection route was not established during retirement".into(),
                ))
            }
            None if connections
                .with_outbound_route(token, |route| {
                    matches!(&route.state, OutboundState::Transitioning)
                })
                .unwrap_or(false) =>
            {
                Ok(RouteRetirement::Retry)
            }
            None => Err(Error::InvalidConfig(
                "connection route generation did not match retirement".into(),
            )),
        }
    }

    fn retire_inbound_route_for_retirement(
        &self,
        connections: &mut ConnectionRegistry,
        encoded: u64,
        connection: ConnectionToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<RouteRetirement> {
        let token = CmRouteToken::decode(encoded);
        match connections.lookup_inbound(token) {
            Lookup::Occupied(_) => {}
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                return Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                });
            }
        }
        let listener = connections.inbound_listener(token);
        let route_state = connections.take_inbound_state_if(token, |route_state| {
            route_state.references_connection(connection)
        });
        match route_state {
            Some(InboundState::EstablishedAwaitingDelivery { request, .. }) => {
                let delivered = request.fail_undelivered_into(Error::DriverShutdown, actions);
                if delivered
                    && let Some(listener) = listener.as_ref().and_then(Weak::upgrade)
                    && listener.finish_selected_route(encoded)
                {
                    self.enqueue_listener_work(&listener);
                }
                connections.release_route(token, true);
                Ok(RouteRetirement::Complete {
                    completion: (!delivered).then(|| InboundRetirementCompletion {
                        listener: listener.clone().unwrap_or_default(),
                        route: encoded,
                        request: None,
                        result: None,
                        selected: true,
                    }),
                    reject: None,
                })
            }
            Some(InboundState::Established { .. }) => {
                connections.release_route(token, true);
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
                connections.release_route(token, true);
                Ok(RouteRetirement::Complete {
                    completion: Some(InboundRetirementCompletion {
                        listener: listener.clone().unwrap_or_default(),
                        route: encoded,
                        request,
                        result: completion,
                        selected,
                    }),
                    reject,
                })
            }
            Some(route_state) => {
                connections.set_inbound_state(token, route_state);
                Err(Error::InvalidConfig(
                    "inbound connection route was not established during retirement".into(),
                ))
            }
            None if connections
                .with_inbound_route(token, |route| {
                    matches!(&route.state, InboundState::Transitioning)
                })
                .unwrap_or(false) =>
            {
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
    pub(in crate::v2::engine) fn terminalize_cm(
        &self,
        connections: &mut ConnectionRegistry,
        outcome: &MemoizedTerminalResult,
    ) {
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        shutdown::terminalize_into(&self.cm, connections, outcome, &mut actions);
        actions.publish();
    }

    pub(in crate::v2::engine) fn terminalize_cm_into(
        &self,
        connections: &mut ConnectionRegistry,
        outcome: &MemoizedTerminalResult,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.cm.terminalize_into(connections, outcome, actions);
    }

    pub(in crate::v2::engine) fn retained_cm_owner_count(
        &self,
        connections: &ConnectionRegistry,
    ) -> usize {
        self.cm.retained_owner_count(connections)
    }

    pub(in crate::v2::engine) fn has_cm_work(&self, connections: &ConnectionRegistry) -> bool {
        self.cm.has_software_work(connections)
    }

    pub(in crate::v2::engine) fn service_cm_software_class(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineResources>,
        class: CmSoftwareClass,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        self.cm.service_software_class_into(
            connections,
            self,
            io_core,
            resources,
            class,
            budget,
            actions,
        )
    }

    pub(in crate::v2::engine) fn cm_software_snapshot(
        &self,
        connections: &ConnectionRegistry,
    ) -> CmSoftwareSnapshot {
        self.cm.software_snapshot(connections)
    }

    pub(in crate::v2::engine) fn try_process_cm_event(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineResources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        self.cm
            .try_process_event(connections, self, io_core, resources, actions)
    }

    pub(in crate::v2::engine) fn has_pending_cm_event(&self) -> bool {
        self.cm.has_pending_event()
    }

    pub(in crate::v2::engine) fn service_deferred_cm_destructions(
        &self,
        connections: &mut ConnectionRegistry,
        io_core: &mut crate::v2::engine::io_core::IoState,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
        defer_one_event: impl FnMut(&ConnectionRegistry) -> Result<bool>,
    ) -> Result<usize> {
        self.cm
            .service_cm_destructions_into(connections, io_core, budget, actions, defer_one_event)
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
        token: ConnectionToken,
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

#[cfg(test)]
enum TestCmDestruction {
    Listener {
        listener: Arc<ListenerState>,
        destroy_error: Option<String>,
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
            Self::Route(_) | Self::Connection { .. } => None,
        }
    }
}

pub(super) struct InboundRoute {
    pub(super) raw_id: usize,
    pub(super) context_key: usize,
    pub(super) listener: Weak<ListenerState>,
    pub(super) state: InboundState,
}

impl InboundRoute {
    pub(super) fn new(_token: CmRouteToken, listener: Weak<ListenerState>) -> Self {
        Self {
            raw_id: 0,
            context_key: 0,
            listener,
            state: InboundState::Transitioning,
        }
    }

    pub(super) fn set_identity(&mut self, raw_id: usize, context_key: usize) {
        self.raw_id = raw_id;
        self.context_key = context_key;
    }

    pub(super) fn set_state(&mut self, state: InboundState) {
        self.state = state;
    }

    pub(super) fn take_state_if(
        &mut self,
        predicate: impl FnOnce(&InboundState) -> bool,
    ) -> Option<InboundState> {
        if !predicate(&self.state) {
            return None;
        }
        Some(std::mem::replace(
            &mut self.state,
            InboundState::Transitioning,
        ))
    }
}

pub(super) enum InboundState {
    PendingSelection {
        cm_id: SharedCmId,
        reservation: ConnectionReservation,
    },
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
            Self::PendingSelection { .. } => false,
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

pub(super) struct OutboundRoute {
    pub(super) token: CmRouteToken,
    pub(super) raw_id: usize,
    pub(super) context_key: usize,
    pub(super) state: OutboundState,
}

impl OutboundRoute {
    pub(super) fn new(token: CmRouteToken, request: Arc<OutboundRequest>) -> Self {
        Self {
            token,
            raw_id: 0,
            context_key: 0,
            state: OutboundState::Transitioning,
        }
        .with_initial_request(request)
    }

    fn with_initial_request(self, request: Arc<OutboundRequest>) -> Self {
        request
            .route_token
            .store(self.token.encode(), Ordering::Release);
        self
    }

    pub(super) fn set_identity(&mut self, raw_id: usize, context_key: usize) {
        self.raw_id = raw_id;
        self.context_key = context_key;
    }

    pub(super) fn set_state(&mut self, state: OutboundState) {
        self.state = state;
    }

    pub(super) fn take_state_if(
        &mut self,
        predicate: impl FnOnce(&OutboundState) -> bool,
    ) -> Option<OutboundState> {
        if !predicate(&self.state) {
            return None;
        }
        Some(std::mem::replace(
            &mut self.state,
            OutboundState::Transitioning,
        ))
    }

    pub(super) fn request(&self) -> Option<Arc<OutboundRequest>> {
        match &self.state {
            OutboundState::AwaitAddr { request, .. }
            | OutboundState::AwaitRoute { request, .. }
            | OutboundState::AwaitEstablished { request, .. }
            | OutboundState::EstablishedAwaitingDelivery { request, .. }
            | OutboundState::DisconnectedAwaitingDelivery { request, .. }
            | OutboundState::FailedAwaitingDelivery { request, .. } => Some(Arc::clone(request)),
            OutboundState::Disconnected { .. }
            | OutboundState::Failed { .. }
            | OutboundState::Closing { .. }
            | OutboundState::Quarantined { .. }
            | OutboundState::Transitioning => None,
        }
    }

    pub(super) fn is_establishing(&self) -> bool {
        matches!(
            &self.state,
            OutboundState::AwaitAddr { .. }
                | OutboundState::AwaitRoute { .. }
                | OutboundState::AwaitEstablished { .. }
                | OutboundState::Transitioning
        )
    }

    pub(super) fn is_disconnected(&self) -> bool {
        matches!(
            &self.state,
            OutboundState::DisconnectedAwaitingDelivery { .. }
                | OutboundState::Disconnected { .. }
                | OutboundState::FailedAwaitingDelivery { .. }
                | OutboundState::Failed { .. }
                | OutboundState::Closing { .. }
                | OutboundState::Quarantined { .. }
        )
    }
}

#[derive(Clone)]
pub(super) struct EstablishedConnectionRoute {
    token: ConnectionToken,
}

impl EstablishedConnectionRoute {
    fn new(token: ConnectionToken) -> Self {
        Self { token }
    }
}

pub(super) enum OutboundState {
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
