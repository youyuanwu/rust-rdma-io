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
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::super::lifecycle::MemoizedTerminalResult;
use super::super::registry::{ConnectionToken, ListenerToken, Lookup};
use super::super::resources::EngineReactorResources;
use super::super::{ConnectionSetup, RdmaConnection, RdmaConnectionConfig};
use super::connection::{
    ConnectionCmRoute, ConnectionReservation, FailedConnectionInstallResources, SharedCmId,
    VerbsConnectionResources, install_reserved_connection, reserve_connection,
};
#[cfg(test)]
use super::listener::RdmaListenerConfig;
use super::listener::{
    AcceptRequest, ChildAdmission, InboundRejectReason, IncomingChild, ListenRequest,
    ListenerAction, ListenerRegistry, RdmaListener, empty_connection_setup,
    run_setup_before_establish, with_validated_listener_backlog,
};
use super::registry::ConnectionRegistry;
use super::{SessionFrontend, SessionManager};
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
    Listener { token: ListenerToken, raw_id: usize },
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
    context_routes: HashMap<usize, ContextRoute>,
    pending_listens: VecDeque<(ListenerToken, Arc<ListenRequest>)>,
    listener_work: VecDeque<ListenerToken>,
    listeners: ListenerRegistry,
    cm_destructions: VecDeque<PendingCmDestruction>,
    non_listener_destructions: usize,
    quarantined_cm_owners: Vec<SharedCmId>,
    pending_event: Option<PendingCmEvent>,
    shutting_down: bool,
}

impl CmState {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            context_routes: HashMap::new(),
            pending_listens: VecDeque::new(),
            listener_work: VecDeque::new(),
            listeners: ListenerRegistry::new(capacity)?,
            cm_destructions: VecDeque::new(),
            non_listener_destructions: 0,
            quarantined_cm_owners: Vec::new(),
            pending_event: None,
            shutting_down: false,
        })
    }

    pub(in crate::v2::engine) fn reserve_listener(
        &mut self,
        request: &ListenRequest,
    ) -> Result<ListenerToken> {
        self.listeners
            .reserve(request.address, request.config.clone())
    }

    pub(in crate::v2::engine) fn listener_slot_available(&self) -> bool {
        self.listeners.has_capacity()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn listener_count(&self) -> usize {
        self.listeners.live()
    }

    pub(in crate::v2::engine) fn enqueue_listen(
        &mut self,
        token: ListenerToken,
        request: Arc<ListenRequest>,
    ) {
        self.pending_listens.push_back((token, request));
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_listen_addresses(&self) -> Vec<std::net::SocketAddr> {
        self.pending_listens
            .iter()
            .map(|(_, request)| request.address)
            .collect()
    }

    pub(in crate::v2::engine) fn enqueue_listener_work(&mut self, token: ListenerToken) {
        let enqueue = self
            .listeners
            .get_mut(token)
            .is_some_and(|listener| listener.try_enqueue_work());
        if enqueue {
            self.listener_work.push_back(token);
        }
    }

    pub(in crate::v2::engine) fn request_listener_close(&mut self, token: ListenerToken) -> bool {
        let changed = self
            .listeners
            .get_mut(token)
            .is_some_and(|listener| listener.request_close());
        if changed {
            self.enqueue_listener_work(token);
        }
        changed
    }

    pub(in crate::v2::engine) fn admit_accept(
        &mut self,
        token: ListenerToken,
        request: Arc<AcceptRequest>,
    ) -> Result<()> {
        self.listeners
            .get_mut(token)
            .ok_or(Error::TransportClosed)?
            .register_waiter(request)
    }

    fn defer_cm_id(&mut self, cm_id: SharedCmId) {
        self.push_cm_destruction_back(PendingCmDestruction::Route(cm_id));
    }

    fn quarantine_cm_id(&mut self, cm_id: SharedCmId) {
        self.quarantined_cm_owners.push(cm_id);
    }

    fn defer_listener_cm_id(&mut self, listener: ListenerToken, cm_id: SharedCmId) {
        if let Some(entry) = self.listeners.get_mut(listener) {
            entry.mark_cm_destruction_pending();
        }
        self.push_cm_destruction_back(PendingCmDestruction::Listener { cm_id, listener });
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn defer_test_listener_destruction(
        &mut self,
        listener: ListenerToken,
        destroy_count: Arc<AtomicUsize>,
    ) {
        if let Some(entry) = self.listeners.get_mut(listener) {
            entry.mark_cm_destruction_pending();
        }
        self.push_cm_destruction_back(PendingCmDestruction::Test {
            destroy_count,
            target: TestCmDestruction::Listener {
                listener,
                destroy_error: None,
            },
        });
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn defer_test_route_destruction(
        &mut self,
        destroy_count: Arc<AtomicUsize>,
    ) {
        self.push_cm_destruction_back(PendingCmDestruction::Test {
            destroy_count,
            target: TestCmDestruction::Route,
        });
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn reserve_test_listener_slot(
        &mut self,
        backlog: usize,
    ) -> ListenerToken {
        let address = "127.0.0.1:1".parse().unwrap();
        let token = self
            .listeners
            .reserve(address, RdmaListenerConfig::default().backlog(backlog))
            .unwrap();
        assert!(self.listeners.activate_identity_for_test(
            token,
            address,
            token.encode() as usize,
            token.encode() as usize
        ));
        token
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn test_listener(
        &mut self,
        manager: &SessionManager,
        backlog: usize,
    ) -> (RdmaListener, ListenerToken) {
        let token = self.reserve_test_listener_slot(backlog);
        let listener = RdmaListener::from_state(manager, self.listeners.get(token).unwrap());
        (listener, token)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn release_test_listener(&mut self, token: ListenerToken) {
        if let Some(listener) = self.listeners.release(token, true) {
            listener.admission().close();
        }
    }

    fn insert_context_route(&mut self, context_key: usize, route: ContextRoute) -> bool {
        match self.context_routes.entry(context_key) {
            Entry::Vacant(entry) => {
                entry.insert(route);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    fn activate_listener_identity(
        &mut self,
        token: ListenerToken,
        local_addr: std::net::SocketAddr,
        cm_id: SharedCmId,
        raw_id: usize,
        context_key: usize,
    ) -> std::result::Result<(), SharedCmId> {
        if self.context_routes.contains_key(&context_key)
            || self.listeners.token_for_raw(raw_id).is_some()
            || self.listeners.get(token).is_none()
        {
            return Err(cm_id);
        }
        self.listeners
            .activate(token, local_addr, cm_id, raw_id, context_key)?;
        self.context_routes
            .insert(context_key, ContextRoute::Listener { token, raw_id });
        Ok(())
    }

    #[cfg(test)]
    fn activate_listener_identity_for_test(
        &mut self,
        token: ListenerToken,
        local_addr: std::net::SocketAddr,
        raw_id: usize,
        context_key: usize,
    ) -> bool {
        if self.context_routes.contains_key(&context_key)
            || self.listeners.token_for_raw(raw_id).is_some()
            || self.listeners.get(token).is_none()
        {
            return false;
        }
        if !self
            .listeners
            .activate_identity_for_test(token, local_addr, raw_id, context_key)
        {
            return false;
        }
        self.context_routes
            .insert(context_key, ContextRoute::Listener { token, raw_id });
        true
    }

    pub(in crate::v2::engine) fn has_software_work(
        &self,
        connections: &ConnectionRegistry,
    ) -> bool {
        let pending = connections.pending_outbound_count() != 0;
        let pending_listens = !self.pending_listens.is_empty();
        let cancellations = connections.cancellation_count() != 0;
        let listener_work = !self.listener_work.is_empty();
        let retirements = connections.retirement_count() != 0;
        let cm_destructions = !self.cm_destructions.is_empty();
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
            + self.pending_listens.len()
            + connections.cancellation_count()
            + self.listener_work.len()
            + connections.retirement_count()
    }

    pub(in crate::v2::engine) fn has_destruction_work(&self) -> bool {
        self.destruction_work_count() != 0
    }

    pub(in crate::v2::engine) fn destruction_work_count(&self) -> usize {
        self.cm_destructions.len()
    }

    pub(in crate::v2::engine) fn listener_work_count(&self) -> usize {
        self.listener_work.len()
    }

    fn take_pending_event(&mut self) -> Option<PendingCmEvent> {
        self.pending_event.take()
    }

    pub(in crate::v2::engine) fn has_pending_event(&self) -> bool {
        self.pending_event.is_some()
    }

    pub(in crate::v2::engine) fn defer_one_event(
        &mut self,
        connections: &ConnectionRegistry,
        resources: &EngineReactorResources,
    ) -> Result<bool> {
        if self.pending_event.is_some() {
            return Ok(true);
        }
        let Some(event) = event::acquire_event(self, connections, resources)? else {
            return Ok(false);
        };
        self.pending_event = Some(event);
        Ok(true)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "bounded source service keeps each reactor-owned input explicit"
    )]
    pub(in crate::v2::engine) fn service_software_class_into(
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
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
                    let pending = self.pending_listens.pop_front();
                    if let Some((token, request)) = pending {
                        self.start_listener(shared, resources, token, request, actions)?;
                        processed += 1;
                    } else {
                        break;
                    }
                }
                CmSoftwareClass::ListenerWork => {
                    let token = self.listener_work.pop_front();
                    if let Some(token) = token {
                        if let Some(listener) = self.listeners.get_mut(token) {
                            listener.begin_work();
                        } else {
                            processed += 1;
                            continue;
                        }
                        self.service_listener(
                            connections,
                            shared,
                            io_core,
                            resources,
                            token,
                            actions,
                        )?;
                        let has_work = self
                            .listeners
                            .get(token)
                            .is_some_and(|listener| listener.has_work());
                        if has_work {
                            self.enqueue_listener_work(token);
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
                self.pending_listens.len(),
                self.listener_work.len(),
            ],
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_software(
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
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
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineReactorResources,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        event::try_process_event(self, connections, shared, io_core, resources, actions)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn begin_shutdown(
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        outcome: &MemoizedTerminalResult,
    ) {
        shutdown::begin(self, connections, shared, outcome);
    }

    pub(in crate::v2::engine) fn start_bounded_shutdown(&mut self) {
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
        &mut self,
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
        connections.live() + self.retained_session_owner_count()
    }

    pub(in crate::v2::engine) fn retained_session_owner_count(&self) -> usize {
        let listeners = self.listeners.live();
        listeners + self.non_listener_destructions + self.quarantined_cm_owners.len()
    }

    fn push_cm_destruction_back(&mut self, pending: PendingCmDestruction) {
        self.non_listener_destructions += usize::from(pending.listener().is_none());
        self.cm_destructions.push_back(pending);
    }

    fn push_cm_destruction_front(&mut self, pending: PendingCmDestruction) {
        self.non_listener_destructions += usize::from(pending.listener().is_none());
        self.cm_destructions.push_front(pending);
    }

    fn pop_cm_destruction_front(&mut self) -> Option<PendingCmDestruction> {
        let pending = self.cm_destructions.pop_front()?;
        self.non_listener_destructions = self
            .non_listener_destructions
            .saturating_sub(usize::from(pending.listener().is_none()));
        Some(pending)
    }

    pub(in crate::v2::engine) fn retained_provider_owner_count(&self) -> usize {
        self.listeners.provider_owner_count()
            + self.cm_destructions.len()
            + self.quarantined_cm_owners.len()
    }

    pub(in crate::v2::engine) fn pending_lifecycle_work_count(&self) -> usize {
        self.pending_listens.len()
            + self.listener_work.len()
            + self.cm_destructions.len()
            + self.quarantined_cm_owners.len()
            + self.listeners.live()
    }

    pub(in crate::v2::engine) fn service_cm_destructions_into(
        &mut self,
        connections: &mut ConnectionRegistry,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineReactorResources,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<usize> {
        retirement::service_cm_destructions(self, connections, io_core, resources, budget, actions)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_cm_destructions_with_probe(
        &mut self,
        connections: &mut ConnectionRegistry,
        io_core: &mut crate::v2::engine::io_core::IoState,
        budget: usize,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
        mut probe: impl FnMut() -> Result<bool>,
    ) -> Result<usize> {
        retirement::service_cm_destructions_with_probe(
            self,
            connections,
            io_core,
            budget,
            actions,
            |_state, _connections| probe(),
        )
    }

    pub(in crate::v2::engine) fn terminalize_into(
        &mut self,
        connections: &mut ConnectionRegistry,
        outcome: &MemoizedTerminalResult,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        shutdown::terminalize_into(self, connections, outcome, actions);
    }

    fn start_listener(
        &mut self,
        shared: &SessionManager,
        resources: &EngineReactorResources,
        token: ListenerToken,
        request: Arc<ListenRequest>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<()> {
        inbound::start_listener(self, shared, resources, token, request, actions)
    }

    fn service_listener(
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: Option<&EngineReactorResources>,
        listener: ListenerToken,
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
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        resources: &EngineReactorResources,
        listener: ListenerToken,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        inbound::handle_connect_request(self, connections, shared, resources, listener, snapshot)
    }

    fn handle_listener_event(
        &mut self,
        shared: &SessionManager,
        listener: ListenerToken,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        inbound::handle_listener_event(self, shared, listener, snapshot)
    }

    fn reject_raw_child(
        &mut self,
        resources: &EngineReactorResources,
        raw_id: usize,
        reason: InboundRejectReason,
    ) -> Result<()> {
        inbound::reject_raw_child(self, resources, raw_id, reason)
    }

    fn start_outbound(
        &mut self,
        connections: &mut ConnectionRegistry,
        resources: &EngineReactorResources,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        outbound::start(self, connections, resources, request, reservation, actions)
    }

    fn process_cancellation(
        &mut self,
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
        &mut self,
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

    #[allow(
        clippy::too_many_arguments,
        reason = "one CM event transaction keeps all reactor-owned state and evidence explicit"
    )]
    fn handle_event(
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        resources: &EngineReactorResources,
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
        &mut self,
        connections: &mut ConnectionRegistry,
        shared: &SessionManager,
        io_core: &mut crate::v2::engine::io_core::IoState,
        token: ConnectionToken,
        snapshot: CmEventSnapshot,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<EventDisposition> {
        inbound::handle_event(self, connections, shared, io_core, token, snapshot, actions)
    }

    pub(super) fn prepare_connection_close(
        &mut self,
        connections: &mut ConnectionRegistry,
        token: ConnectionToken,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<bool> {
        let Some(ConnectionCmRoute::Inbound(encoded)) = connections.connection_route(token) else {
            return Ok(true);
        };
        inbound::prepare_selected_connection_close(self, connections, encoded, token, actions)
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
        &mut self,
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
                    disposition: RouteRetirementDisposition::None,
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
                    disposition: RouteRetirementDisposition::None,
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
        &mut self,
        connections: &mut ConnectionRegistry,
        encoded: u64,
        connection: ConnectionToken,
        _actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> Result<RouteRetirement> {
        let token = CmRouteToken::decode(encoded);
        match connections.lookup_inbound(token) {
            Lookup::Occupied(_) => {}
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
                return Ok(RouteRetirement::Complete {
                    completion: None,
                    disposition: RouteRetirementDisposition::None,
                });
            }
        }
        let listener = connections.inbound_listener(token);
        let route_state = connections.take_inbound_state_if(token, |route_state| {
            route_state.references_connection(connection)
        });
        match route_state {
            Some(route_state @ InboundState::EstablishedAwaitingDelivery { .. }) => {
                connections.set_inbound_state(token, route_state);
                Err(Error::InvalidConfig(
                    "selected accepted route reached retirement before close preparation".into(),
                ))
            }
            Some(InboundState::Established { .. }) => {
                connections.release_route(token, true);
                Ok(RouteRetirement::Complete {
                    completion: None,
                    disposition: RouteRetirementDisposition::None,
                })
            }
            Some(InboundState::Closing {
                request,
                completion,
                selected,
                peer,
                ..
            }) => {
                let disposition = match peer {
                    InboundPeerState::PreAccept {
                        reject: Some(reason),
                    } => RouteRetirementDisposition::Reject(reason),
                    InboundPeerState::PreAccept { reject: None } | InboundPeerState::Accepted => {
                        RouteRetirementDisposition::None
                    }
                };
                connections.release_route(token, true);
                Ok(RouteRetirement::Complete {
                    completion: Some(InboundRetirementCompletion {
                        listener,
                        route: encoded,
                        request,
                        result: completion,
                        selected,
                    }),
                    disposition,
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

    fn remove_owned_context_route(&mut self, cm_id: Option<&SharedCmId>) {
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
        &mut self,
        context_key: usize,
        raw_id: usize,
        route_token: Option<u64>,
    ) -> bool {
        let Some(route_token) = route_token else {
            return false;
        };
        let owned = match self.context_routes.get(&context_key).copied() {
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
            }) => token.encode() == route_token && owner == raw_id,
            None => false,
        };
        if owned {
            self.context_routes.remove(&context_key);
        }
        owned
    }
}

fn build_qp(
    resources: &EngineReactorResources,
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
        disposition: RouteRetirementDisposition,
    },
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteRetirementDisposition {
    None,
    Reject(InboundRejectReason),
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
        listener: ListenerToken,
    },
    #[cfg(test)]
    Test {
        destroy_count: Arc<AtomicUsize>,
        target: TestCmDestruction,
    },
}

#[cfg(test)]
enum TestCmDestruction {
    Route,
    Listener {
        listener: ListenerToken,
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

    fn listener(&self) -> Option<ListenerToken> {
        match self {
            Self::Listener { listener, .. } => Some(*listener),
            #[cfg(test)]
            Self::Test {
                target: TestCmDestruction::Listener { listener, .. },
                ..
            } => Some(*listener),
            #[cfg(test)]
            Self::Test {
                target: TestCmDestruction::Route,
                ..
            } => None,
            Self::Route(_) | Self::Connection { .. } => None,
        }
    }
}

pub(super) struct InboundRoute {
    pub(super) raw_id: usize,
    pub(super) context_key: usize,
    pub(super) listener: ListenerToken,
    pub(super) state: InboundState,
}

impl InboundRoute {
    pub(super) fn new(_token: CmRouteToken, listener: ListenerToken) -> Self {
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
        peer: InboundPeerState,
    },
    Quarantined {
        connection: Option<EstablishedConnectionRoute>,
    },
    Transitioning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InboundPeerState {
    PreAccept { reject: Option<InboundRejectReason> },
    Accepted,
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
    listener: Option<ListenerToken>,
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
