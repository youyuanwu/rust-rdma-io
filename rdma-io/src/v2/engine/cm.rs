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
use rdma_io_sys::rdmacm::rdma_cm_id;

#[cfg(test)]
use super::SetupSummary;
use super::connection::{
    ConnectionCmRoute, ConnectionReservation, ConnectionState, SharedCmId,
    VerbsConnectionResources, WorkRequestPoster, install_reserved_connection, reserve_connection,
};
use super::diagnostics::CmEventReject;
use super::listener::{
    AcceptRequest, EmptyPreEstablishSetup, InboundRejectReason, IncomingChild,
    KERNEL_LISTEN_BACKLOG_REQUEST, ListenRequest, ListenerAction, ListenerState, RdmaListener,
    run_setup_before_establish,
};
use super::registry::{ConnectionToken, Lookup, PagedRegistry, RegistryToken, lock_unpoison};
use super::resources::EngineResources;
use super::{EngineOutcome, EngineShared, PreEstablishSetup, RdmaConnection, RdmaConnectionConfig};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextRoute {
    Outbound { token: CmRouteToken, raw_id: usize },
    Inbound { token: CmRouteToken, raw_id: usize },
    Listener { token: u64, raw_id: usize },
}

pub(super) struct CmState {
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
    shutting_down: AtomicBool,
}

impl CmState {
    pub(super) fn new(capacity: usize) -> Result<Self> {
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
            shutting_down: AtomicBool::new(false),
        })
    }

    fn enqueue(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.pending).push_back(request);
    }

    pub(super) fn enqueue_listen(&self, request: Arc<ListenRequest>) {
        lock_unpoison(&self.pending_listens).push_back(request);
    }

    pub(super) fn enqueue_listener_work(&self, listener: &Arc<ListenerState>) {
        if listener.try_enqueue_work() {
            lock_unpoison(&self.listener_work).push_back(Arc::clone(listener));
        }
    }

    fn defer_cm_id(&self, cm_id: SharedCmId) {
        lock_unpoison(&self.cm_destructions).push_back(PendingCmDestruction::Route(cm_id));
    }

    #[cfg(test)]
    pub(super) fn defer_test_listener_destruction(
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

    pub(super) fn mark_accept_delivered(
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

    pub(super) fn has_software_work(&self) -> bool {
        !lock_unpoison(&self.pending).is_empty()
            || !lock_unpoison(&self.pending_listens).is_empty()
            || !lock_unpoison(&self.cancellations).is_empty()
            || !lock_unpoison(&self.listener_work).is_empty()
            || !lock_unpoison(&self.retirements).is_empty()
            || !lock_unpoison(&self.cm_destructions).is_empty()
    }

    pub(super) fn service_software(
        &self,
        shared: &Arc<EngineShared>,
        resources: Option<&EngineResources>,
        budget: usize,
    ) -> Result<usize> {
        // Snapshot each class depth at pass entry. Work requeued while it is
        // transitioning is therefore deferred to a later driver poll instead
        // of consuming this pass's bounded budget repeatedly.
        let mut remaining = [
            lock_unpoison(&self.cancellations).len(),
            lock_unpoison(&self.retirements).len(),
            lock_unpoison(&self.pending).len(),
            lock_unpoison(&self.pending_listens).len(),
            lock_unpoison(&self.listener_work).len(),
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
                3 => {
                    let request = { lock_unpoison(&self.pending_listens).pop_front() };
                    if let Some(request) = request {
                        let resources = resources.ok_or_else(|| {
                            Error::InvalidConfig(
                                "listener creation requires live engine resources".into(),
                            )
                        })?;
                        self.start_listener(shared, resources, request)?;
                        processed += 1;
                    }
                }
                4 => {
                    let listener = { lock_unpoison(&self.listener_work).pop_front() };
                    if let Some(listener) = listener {
                        listener.begin_work();
                        let resources = resources.ok_or_else(|| {
                            Error::InvalidConfig(
                                "listener progress requires live engine resources".into(),
                            )
                        })?;
                        self.service_listener(shared, resources, &listener)?;
                        if listener.has_work() {
                            self.enqueue_listener_work(&listener);
                        }
                        processed += 1;
                    }
                }
                _ => unreachable!("software work has five classes"),
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
        let route = self.lookup_dispatch_route(snapshot);
        event.ack_checked().map_err(Error::from)?;
        shared
            .diagnostic_counters
            .cm_events_processed
            .fetch_add(1, Ordering::Relaxed);

        let route = match route {
            Ok(route) => route,
            Err(reject) => {
                shared.diagnostic_counters.reject_cm_event(reject);
                if snapshot.event_type == CmEventType::ConnectRequest {
                    self.reject_raw_child(
                        shared,
                        resources,
                        snapshot.id,
                        InboundRejectReason::ListenerClosed,
                    )?;
                }
                return Ok(true);
            }
        };
        let disposition = match route {
            CmDispatchRoute::Outbound(route) => {
                self.handle_event(shared, resources, &route, snapshot)?
            }
            CmDispatchRoute::Inbound(route) => {
                self.handle_inbound_event(shared, &route, snapshot)?
            }
            CmDispatchRoute::Listener(listener) => {
                if snapshot.event_type == CmEventType::ConnectRequest {
                    self.handle_connect_request(shared, resources, &listener, snapshot)?
                } else {
                    self.handle_listener_event(shared, &listener, snapshot)?
                }
            }
        };
        match disposition {
            EventDisposition::Handled => {}
            EventDisposition::Rejected(reject) => {
                shared.diagnostic_counters.reject_cm_event(reject);
            }
        }
        Ok(true)
    }

    pub(super) fn begin_shutdown(&self, shared: &Arc<EngineShared>, outcome: &EngineOutcome) {
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
        let pending_listens: Vec<_> = lock_unpoison(&self.pending_listens).drain(..).collect();
        for request in pending_listens {
            request.complete(Err(terminal_error(outcome)));
        }
        let listeners: Vec<_> = lock_unpoison(&self.listeners).values().cloned().collect();
        for listener in listeners {
            listener.request_close(shared);
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
            + lock_unpoison(&self.pending_listens).len()
            + lock_unpoison(&self.cancellations).len()
            + lock_unpoison(&self.listener_work).len()
            + lock_unpoison(&self.retirements).len()
            + lock_unpoison(&self.cm_destructions).len()
            + self.inbound_routes.live()
            + lock_unpoison(&self.listeners).len()
    }

    pub(super) fn retained_owner_count(&self) -> usize {
        self.routes.live()
            + self.inbound_routes.live()
            + lock_unpoison(&self.listeners).len()
            + lock_unpoison(&self.cm_destructions).len()
    }

    pub(super) fn listener_counts(&self) -> (usize, usize, usize, usize) {
        let listeners: Vec<_> = lock_unpoison(&self.listeners).values().cloned().collect();
        let mut queued_children = 0;
        let mut pending_accepts = 0;
        let mut selected_accepts = 0;
        for listener in &listeners {
            let (children, waiters, selected) = listener.queue_counts();
            queued_children += children;
            pending_accepts += waiters;
            selected_accepts += selected;
        }
        (
            listeners.len(),
            queued_children,
            pending_accepts,
            selected_accepts,
        )
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
                    #[cfg(any(test, feature = "test-hooks"))]
                    if let Some(cm_id) = pending.cm_id() {
                        crate::test_support::destruction::record(
                            crate::test_support::destruction::DestructionKind::CmDrainToWouldBlock,
                            cm_id.as_raw() as usize,
                        );
                    }
                    self.remove_owned_context_route(pending.cm_id());
                    match pending {
                        PendingCmDestruction::Route(cm_id) => cm_id.destroy()?,
                        PendingCmDestruction::Connection {
                            cm_id,
                            connection,
                            completion,
                        } => {
                            let destroy_result = cm_id.destroy().map_err(|error| {
                                contextual_cm_error(
                                    format!(
                                        "destroy connection CM ID for slot {} generation {}",
                                        connection.token.slot, connection.token.generation
                                    ),
                                    error,
                                )
                            });
                            let finalize_result =
                                self.release_connection_retirement(shared, &connection);
                            self.complete_connection_cm_destruction(
                                shared,
                                connection,
                                completion,
                                destroy_result,
                                finalize_result,
                            )?;
                        }
                        PendingCmDestruction::Listener { cm_id, listener } => {
                            let destroy_result = cm_id.destroy().map_err(|error| {
                                contextual_cm_error(
                                    format!("destroy listener CM ID for {}", listener.local_addr),
                                    error,
                                )
                            });
                            Self::complete_listener_cm_destruction(listener, destroy_result)?;
                        }
                        #[cfg(test)]
                        PendingCmDestruction::Test {
                            destroy_count,
                            target,
                        } => {
                            destroy_count.fetch_add(1, Ordering::AcqRel);
                            match target {
                                TestCmDestruction::Listener {
                                    listener,
                                    destroy_error,
                                } => Self::complete_listener_cm_destruction(
                                    listener,
                                    injected_cm_result(destroy_error),
                                )?,
                                TestCmDestruction::Connection {
                                    connection,
                                    completion,
                                    destroy_error,
                                    finalize_error,
                                } => {
                                    let finalize_result = match finalize_error {
                                        Some(error) => injected_cm_result(Some(error)),
                                        None => {
                                            self.release_connection_retirement(shared, &connection)
                                        }
                                    };
                                    self.complete_connection_cm_destruction(
                                        shared,
                                        connection,
                                        completion,
                                        injected_cm_result(destroy_error),
                                        finalize_result,
                                    )?
                                }
                            }
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

    fn complete_listener_cm_destruction(
        listener: Arc<ListenerState>,
        result: Result<()>,
    ) -> Result<()> {
        match result {
            Ok(()) => {
                listener.finish_close(None);
                Ok(())
            }
            Err(error) => {
                listener.finish_close(Some(error.clone()));
                Err(error)
            }
        }
    }

    fn complete_connection_cm_destruction(
        &self,
        shared: &EngineShared,
        connection: Arc<ConnectionState>,
        completion: Option<InboundRetirementCompletion>,
        destroy_result: Result<()>,
        finalize_result: Result<()>,
    ) -> Result<()> {
        match (destroy_result, finalize_result) {
            (Ok(()), Ok(())) => {
                connection.finish_retirement();
                shared.record_connection_retired(&connection);
                self.finish_inbound_retirement(completion);
                Ok(())
            }
            (destroy_result, finalize_result) => {
                let error = connection_destruction_error(destroy_result, finalize_result);
                let message = error_detail(&error);
                connection.fail_retirement(error.clone());
                shared.record_connection_retirement_failure(&connection);
                self.fail_inbound_retirement(completion, message);
                Err(error)
            }
        }
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
        let pending_listens: Vec<_> = lock_unpoison(&self.pending_listens).drain(..).collect();
        for request in pending_listens {
            request.complete(Err(terminal_error(outcome)));
        }
        let mut listeners: Vec<_> = lock_unpoison(&self.listeners).values().cloned().collect();
        let pending_listeners: Vec<_> = lock_unpoison(&self.cm_destructions)
            .iter()
            .filter_map(PendingCmDestruction::listener)
            .cloned()
            .collect();
        for listener in pending_listeners {
            if !listeners
                .iter()
                .any(|active| Arc::ptr_eq(active, &listener))
            {
                listeners.push(listener);
            }
        }
        for listener in listeners {
            listener.terminalize(outcome);
        }
    }

    fn start_listener(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        request: Arc<ListenRequest>,
    ) -> Result<()> {
        if request.is_cancelled()
            || self.shutting_down.load(Ordering::Acquire)
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            request.complete(Err(Error::DriverShutdown));
            return Ok(());
        }
        let token = self
            .next_listener_token
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| Error::CapacityExhausted)?;
        let cm_id = match CmId::new_with_context_token(
            &resources.cm_event_channel,
            PortSpace::Tcp,
            token,
        ) {
            Ok(cm_id) => SharedCmId::new(cm_id, Arc::clone(&resources.cm_event_channel)),
            Err(error) => {
                request.complete(Err(contextual_cm_error(
                    format!("create listener {}", request.address),
                    error.into(),
                )));
                return Ok(());
            }
        };
        let context_key = cm_id.context_key();
        let raw_id = cm_id.as_raw() as usize;
        if !self.insert_context_route(context_key, ContextRoute::Listener { token, raw_id }) {
            self.defer_cm_id(cm_id);
            request.complete(Err(Error::InvalidConfig(
                "duplicate listener CM context identity".into(),
            )));
            return Ok(());
        }
        if let Err(error) = cm_id.listen(&request.address, KERNEL_LISTEN_BACKLOG_REQUEST) {
            self.defer_cm_id(cm_id);
            request.complete(Err(contextual_cm_error(
                format!(
                    "listen on {} with requested kernel backlog {}",
                    request.address, KERNEL_LISTEN_BACKLOG_REQUEST
                ),
                error.into(),
            )));
            return Ok(());
        }
        let local_addr = cm_id.local_addr().ok_or_else(|| {
            Error::InvalidConfig(format!(
                "listener {} has no local address after rdma_listen",
                request.address
            ))
        });
        let local_addr = match local_addr {
            Ok(local_addr) => local_addr,
            Err(error) => {
                self.defer_cm_id(cm_id);
                request.complete(Err(error));
                return Ok(());
            }
        };
        let state = Arc::new(ListenerState::new(
            token,
            local_addr,
            request.config.clone(),
            cm_id,
        ));
        if !self.insert_listener_identity(token, raw_id, Arc::clone(&state)) {
            let cm_id = state
                .take_cm_id()
                .expect("new duplicate listener still owns its CM ID");
            self.defer_cm_id(cm_id);
            request.complete(Err(Error::InvalidConfig(
                "duplicate listener route identity".into(),
            )));
            return Ok(());
        }
        shared
            .diagnostic_counters
            .listeners_created
            .fetch_add(1, Ordering::Relaxed);
        if request.is_cancelled() {
            state.request_close(shared);
            request.complete(Err(Error::DriverShutdown));
        } else {
            request.complete(Ok(RdmaListener {
                shared: Arc::clone(shared),
                state,
            }));
        }
        Ok(())
    }

    fn service_listener(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
    ) -> Result<()> {
        match listener.next_action() {
            ListenerAction::CancelledBeforeSelection(request) => {
                if request.mark_cancellation_counted() {
                    shared
                        .diagnostic_counters
                        .accept_cancellations_before_selection
                        .fetch_add(1, Ordering::Relaxed);
                }
                request.complete(Err(Error::DriverShutdown));
            }
            ListenerAction::FailUnselected(request) => {
                request.complete(Err(listener.close_error()));
            }
            ListenerAction::RejectChild(child, reason) => {
                self.reject_child(shared, child, reason)?;
            }
            ListenerAction::ProcessSelected { request, child } => {
                self.process_selected_pair(shared, resources, listener, request, child)?;
            }
            ListenerAction::RejectSelected {
                request,
                child,
                reason,
            } => {
                self.reject_child(shared, child, InboundRejectReason::ListenerClosed)?;
                request.complete(Err(reason));
                listener.finish_selected_request(&request);
            }
            ListenerAction::CancelAfterAccept { request, route } => {
                if request.is_cancelled() && request.mark_cancellation_counted() {
                    shared
                        .diagnostic_counters
                        .accept_cancellations_after_selection
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.cancel_inbound_route(shared, route)?;
            }
            ListenerAction::FinalizeClose => {
                self.finalize_listener(listener)?;
            }
            ListenerAction::None => {}
        }
        Ok(())
    }

    fn handle_connect_request(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        if snapshot.status != 0 || listener.is_closing() {
            self.reject_raw_child(
                shared,
                resources,
                snapshot.id,
                InboundRejectReason::ListenerClosed,
            )?;
            return Ok(EventDisposition::Handled);
        }
        let raw = snapshot.id as *mut rdma_cm_id;
        if raw.is_null() {
            return Ok(EventDisposition::Rejected(CmEventReject::Unknown));
        }
        let child_id = unsafe { CmId::from_raw(raw, true) };
        let child_id = SharedCmId::new(child_id, Arc::clone(&resources.cm_event_channel));
        if let Err(error) = child_id.require_context(resources.context.inner()) {
            tracing::warn!(
                listener = %listener.local_addr,
                "rejecting inbound child with mismatched verbs context: {error}"
            );
            self.reject_unreserved_child(shared, child_id, InboundRejectReason::ContextMismatch)?;
            return Ok(EventDisposition::Handled);
        }

        let (admission, reservation) = match reserve_connection(shared) {
            Ok(value) => value,
            Err(error) => {
                let reason = if matches!(error, Error::CapacityExhausted) {
                    InboundRejectReason::ConnectionCapacity
                } else {
                    InboundRejectReason::AdmissionClosed
                };
                self.reject_unreserved_child(shared, child_id, reason)?;
                return Ok(EventDisposition::Handled);
            }
        };
        let admitted = listener.admit_child(IncomingChild::new(child_id, reservation));
        drop(admission);
        for request in admitted.cancelled {
            if request.mark_cancellation_counted() {
                shared
                    .diagnostic_counters
                    .accept_cancellations_before_selection
                    .fetch_add(1, Ordering::Relaxed);
            }
            request.complete(Err(Error::DriverShutdown));
        }
        if let Some((child, reason)) = admitted.rejected {
            self.reject_child(shared, child, reason)?;
        } else {
            self.enqueue_listener_work(listener);
        }
        Ok(EventDisposition::Handled)
    }

    fn handle_listener_event(
        &self,
        shared: &Arc<EngineShared>,
        listener: &Arc<ListenerState>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        if !is_failure_event(snapshot.event_type) && snapshot.status == 0 {
            return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
        }
        let message = format!(
            "listener {} RDMA CM {:?} failed with status {} for id={:#x}",
            listener.local_addr, snapshot.event_type, snapshot.status, snapshot.id
        );
        if snapshot.event_type == CmEventType::DeviceRemoval {
            return Err(Error::Verbs(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                message,
            )));
        }
        listener.fail(shared, Error::Verbs(std::io::Error::other(message)));
        Ok(EventDisposition::Handled)
    }

    fn process_selected_pair(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        listener: &Arc<ListenerState>,
        request: Arc<AcceptRequest>,
        child: IncomingChild,
    ) -> Result<()> {
        if request.is_cancelled()
            || listener.is_closing()
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            if request.is_cancelled() && request.mark_cancellation_counted() {
                shared
                    .diagnostic_counters
                    .accept_cancellations_after_selection
                    .fetch_add(1, Ordering::Relaxed);
            }
            let error = if listener.is_closing() {
                listener.close_error()
            } else {
                Error::DriverShutdown
            };
            let reject = if listener.is_closing() || request.is_cancelled() {
                InboundRejectReason::ListenerClosed
            } else {
                InboundRejectReason::AdmissionClosed
            };
            self.reject_child(shared, child, reject)?;
            request.complete(Err(error));
            listener.finish_selected_request(&request);
            return Ok(());
        }
        let intent = request.take_intent().ok_or_else(|| {
            Error::InvalidConfig("selected accept intent was consumed more than once".into())
        })?;
        let (config, setup) = intent.into_parts()?;
        let (mut child_cm_id, child_reservation) = child.into_resources()?;
        let (token, route) = self
            .inbound_routes
            .allocate_with(|token| Arc::new(InboundRoute::new(token, Arc::downgrade(listener))))?;
        request.set_route_token(token.encode());
        if let Err(error) = child_cm_id.install_context_token(token.encode()) {
            self.inbound_routes.release(token, false);
            self.reject_unreserved_child(shared, child_cm_id, InboundRejectReason::SetupFailure)?;
            drop(child_reservation);
            request.complete(Err(error));
            listener.finish_selected_request(&request);
            return Ok(());
        }
        let raw_id = child_cm_id.as_raw() as usize;
        let context_key = child_cm_id.context_key();
        route.set_identity(raw_id, context_key);
        if !self.insert_context_route(context_key, ContextRoute::Inbound { token, raw_id }) {
            self.inbound_routes.release(token, false);
            self.reject_unreserved_child(shared, child_cm_id, InboundRejectReason::SetupFailure)?;
            drop(child_reservation);
            request.complete(Err(Error::InvalidConfig(
                "duplicate inbound CM context identity".into(),
            )));
            listener.finish_selected_request(&request);
            return Ok(());
        }
        listener.route_selected(&request, token.encode())?;

        let local_addr = child_cm_id.local_addr();
        let peer_addr = child_cm_id.peer_addr();
        let qp = match build_qp(resources, &child_cm_id, &config) {
            Ok(qp) => qp,
            Err(error) => {
                self.remove_owned_context_route(Some(&child_cm_id));
                self.inbound_routes.release(token, false);
                self.reject_unreserved_child(
                    shared,
                    child_cm_id,
                    InboundRejectReason::SetupFailure,
                )?;
                drop(child_reservation);
                request.complete(Err(contextual_cm_error(
                    format!("build inbound QP for {}", listener.local_addr),
                    error,
                )));
                listener.finish_selected_route(token.encode());
                return Ok(());
            }
        };
        let verbs = Arc::new(VerbsConnectionResources::new_shared(qp, child_cm_id));
        let connection = match install_reserved_connection(
            shared,
            Arc::clone(&verbs) as Arc<_>,
            config.clone(),
            local_addr,
            peer_addr,
            child_reservation,
            Some(ConnectionCmRoute::Inbound(token.encode())),
        ) {
            Ok(connection) => connection,
            Err(error) => {
                let cm_id = verbs.destroy_connection();
                if let Some(cm_id) = cm_id {
                    self.reject_unreserved_child(shared, cm_id, InboundRejectReason::SetupFailure)?;
                }
                self.inbound_routes.release(token, false);
                request.complete(Err(error));
                listener.finish_selected_route(token.encode());
                return Ok(());
            }
        };

        let conn_param = match config.conn_param() {
            Ok(param) => param,
            Err(error) => {
                drop(verbs);
                self.fail_selected_connection(shared, &route, request, connection, error)?;
                return Ok(());
            }
        };
        let establish = run_setup_before_establish(
            setup,
            &connection,
            || {
                if request.is_cancelled()
                    || listener.is_closing()
                    || shared.shutdown_requested.load(Ordering::Acquire)
                {
                    Err(if listener.is_closing() {
                        listener.close_error()
                    } else {
                        Error::DriverShutdown
                    })
                } else {
                    Ok(())
                }
            },
            || verbs.accept(&conn_param),
        );
        if let Err(error) = establish {
            drop(verbs);
            self.fail_selected_connection(shared, &route, request, connection, error)?;
            return Ok(());
        }
        drop(verbs);
        route.set_state(InboundState::AwaitEstablished {
            request,
            connection,
        });
        Ok(())
    }

    fn fail_selected_connection(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<InboundRoute>,
        request: Arc<AcceptRequest>,
        connection: RdmaConnection,
        error: Error,
    ) -> Result<()> {
        let connection_state = Arc::clone(&connection.state);
        let teardown = matches!(error, Error::DriverShutdown | Error::TransportClosed);
        let reject = match &error {
            Error::DriverShutdown if !request.is_cancelled() => {
                InboundRejectReason::AdmissionClosed
            }
            Error::DriverShutdown | Error::TransportClosed => InboundRejectReason::ListenerClosed,
            _ => InboundRejectReason::SetupFailure,
        };
        if request.is_cancelled() {
            if request.mark_cancellation_counted() {
                shared
                    .diagnostic_counters
                    .accept_cancellations_after_selection
                    .fetch_add(1, Ordering::Relaxed);
            }
        } else if !teardown {
            shared
                .diagnostic_counters
                .accept_setup_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        route.set_state(InboundState::Closing {
            connection: EstablishedConnectionRoute::new(&connection_state),
            request: Some(request),
            completion: Some(error),
            selected: true,
            reject: Some(reject),
        });
        shared.begin_connection_close(&connection_state);
        drop(connection);
        if connection_state.accepted_count() == 0 {
            self.retire_registered_connection(shared, connection_state.token)?;
        }
        Ok(())
    }

    fn reject_raw_child(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        raw_id: usize,
        reason: InboundRejectReason,
    ) -> Result<()> {
        if raw_id == 0 {
            return Ok(());
        }
        let cm_id = unsafe { CmId::from_raw(raw_id as *mut rdma_cm_id, true) };
        self.reject_unreserved_child(
            shared,
            SharedCmId::new(cm_id, Arc::clone(&resources.cm_event_channel)),
            reason,
        )
    }

    fn reject_unreserved_child(
        &self,
        shared: &Arc<EngineShared>,
        cm_id: SharedCmId,
        reason: InboundRejectReason,
    ) -> Result<()> {
        self.record_inbound_reject(shared, reason);
        cm_id.reject(&[]).map_err(|error| {
            contextual_cm_error(format!("reject inbound child ({reason:?})"), error.into())
        })?;
        self.defer_cm_id(cm_id);
        Ok(())
    }

    fn reject_child(
        &self,
        shared: &Arc<EngineShared>,
        child: IncomingChild,
        reason: InboundRejectReason,
    ) -> Result<()> {
        let (cm_id, reservation) = child.into_resources()?;
        let result = self.reject_unreserved_child(shared, cm_id, reason);
        drop(reservation);
        result
    }

    fn record_inbound_reject(&self, shared: &EngineShared, reason: InboundRejectReason) {
        shared
            .diagnostic_counters
            .inbound_requests_rejected
            .fetch_add(1, Ordering::Relaxed);
        let counter = match reason {
            InboundRejectReason::BacklogFull => {
                &shared.diagnostic_counters.inbound_rejected_backlog_full
            }
            InboundRejectReason::ConnectionCapacity => {
                &shared
                    .diagnostic_counters
                    .inbound_rejected_connection_capacity
            }
            InboundRejectReason::AdmissionClosed => {
                &shared.diagnostic_counters.inbound_rejected_admission_closed
            }
            InboundRejectReason::ListenerClosed => {
                &shared.diagnostic_counters.inbound_rejected_listener_closed
            }
            InboundRejectReason::ContextMismatch => {
                &shared.diagnostic_counters.inbound_rejected_context_mismatch
            }
            InboundRejectReason::SetupFailure => {
                &shared.diagnostic_counters.inbound_rejected_setup_failure
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn cancel_inbound_route(&self, shared: &Arc<EngineShared>, encoded: u64) -> Result<()> {
        let token = CmRouteToken::decode(encoded);
        let Lookup::Occupied(route) = self.inbound_routes.lookup_cloned(token) else {
            return Ok(());
        };
        let state = route.take_state_if(|state| {
            matches!(
                state,
                InboundState::AwaitEstablished { .. }
                    | InboundState::EstablishedAwaitingDelivery { .. }
            )
        });
        let Some(state) = state else {
            return Ok(());
        };
        let cancellation_error = || {
            route
                .listener
                .upgrade()
                .filter(|listener| listener.is_closing())
                .map_or(Error::DriverShutdown, |listener| listener.close_error())
        };
        match state {
            InboundState::AwaitEstablished {
                request,
                connection,
            } => {
                let connection_state = Arc::clone(&connection.state);
                let error = cancellation_error();
                route.set_state(InboundState::Closing {
                    connection: EstablishedConnectionRoute::new(&connection_state),
                    request: Some(request),
                    completion: Some(error),
                    selected: true,
                    reject: None,
                });
                shared.begin_connection_close(&connection_state);
                drop(connection);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            InboundState::EstablishedAwaitingDelivery {
                request,
                connection,
            } => {
                let Some(connection_state) = connection.upgrade() else {
                    self.inbound_routes.release(token, true);
                    return Ok(());
                };
                let error = cancellation_error();
                if request.fail_undelivered(error) {
                    route.set_state(InboundState::Established {
                        connection: connection.clone(),
                    });
                    if let Some(listener) = route.listener.upgrade()
                        && listener.finish_selected_route(encoded)
                    {
                        self.enqueue_listener_work(&listener);
                    }
                    return Ok(());
                }
                route.set_state(InboundState::Closing {
                    connection: connection.clone(),
                    request: None,
                    completion: None,
                    selected: true,
                    reject: None,
                });
                shared.begin_connection_close(&connection_state);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            _ => unreachable!("inbound cancellation state was pre-filtered"),
        }
        Ok(())
    }

    fn finalize_listener(&self, listener: &Arc<ListenerState>) -> Result<()> {
        let Some(cm_id) = listener.take_cm_id() else {
            return Ok(());
        };
        let raw_id = cm_id.as_raw() as usize;
        let mut listeners = lock_unpoison(&self.listeners);
        let mut listener_ids = lock_unpoison(&self.listener_ids);
        let owned = listeners
            .get(&listener.token)
            .is_some_and(|current| Arc::ptr_eq(current, listener));
        if owned {
            listeners.remove(&listener.token);
            if listener_ids.get(&raw_id) == Some(&listener.token) {
                listener_ids.remove(&raw_id);
            }
        }
        drop(listener_ids);
        drop(listeners);
        lock_unpoison(&self.cm_destructions).push_back(PendingCmDestruction::Listener {
            cm_id,
            listener: Arc::clone(listener),
        });
        Ok(())
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
        let raw_id = cm_id.as_raw() as usize;
        if !self.insert_context_route(
            context_key,
            ContextRoute::Outbound {
                token: context_route,
                raw_id,
            },
        ) {
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
        shared.begin_connection_close(&connection_state);
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
        if connection.accepted_count() != 0 {
            return Ok(());
        }
        if !connection.error_transition_complete() {
            self.enqueue_retirement(token);
            return Ok(());
        }
        if !connection.try_begin_retirement() {
            return Ok(());
        }
        let retirement = match connection.cm_route() {
            Some(ConnectionCmRoute::Outbound(encoded)) => {
                self.retire_outbound_connection_route(encoded, &connection)?
            }
            Some(ConnectionCmRoute::Inbound(encoded)) => {
                self.retire_inbound_connection_route(shared, encoded, &connection)?
            }
            None => RouteRetirement::Complete {
                completion: None,
                reject: None,
            },
        };
        let RouteRetirement::Complete { completion, reject } = retirement else {
            connection.retry_retirement();
            self.enqueue_retirement(token);
            return Ok(());
        };
        let cm_id = connection.destroy_connection_resources();
        shared.record_qp_destroy();
        if let Some(cm_id) = cm_id {
            if let Some(reason) = reject {
                self.record_inbound_reject(shared, reason);
                cm_id.reject(&[]).map_err(|error| {
                    contextual_cm_error(
                        "reject selected inbound child after setup rollback",
                        error.into(),
                    )
                })?;
            }
            lock_unpoison(&self.cm_destructions).push_back(PendingCmDestruction::Connection {
                cm_id,
                connection,
                completion,
            });
            return Ok(());
        }
        self.finalize_connection_retirement(shared, connection)?;
        self.finish_inbound_retirement(completion);
        Ok(())
    }

    fn finalize_connection_retirement(
        &self,
        shared: &EngineShared,
        connection: Arc<ConnectionState>,
    ) -> Result<()> {
        self.release_connection_retirement(shared, &connection)?;
        connection.finish_retirement();
        shared.record_connection_retired(&connection);
        Ok(())
    }

    fn release_connection_retirement(
        &self,
        shared: &EngineShared,
        connection: &Arc<ConnectionState>,
    ) -> Result<()> {
        let released = shared
            .connections
            .release(connection.token, connection.qp_num())
            .ok_or_else(|| {
                Error::InvalidConfig("connection registry retirement lost its entry".into())
            })?;
        if !Arc::ptr_eq(&released, connection) {
            return Err(Error::InvalidConfig(
                "connection registry retired a mismatched generation".into(),
            ));
        }
        connection.release_admission();
        Ok(())
    }

    fn finish_inbound_retirement(&self, completion: Option<InboundRetirementCompletion>) {
        let Some(completion) = completion else {
            return;
        };
        if let Some(request) = completion.request
            && let Some(result) = completion.result
        {
            request.complete(Err(result));
        }
        if completion.selected
            && let Some(listener) = completion.listener.upgrade()
            && listener.finish_selected_route(completion.route)
        {
            self.enqueue_listener_work(&listener);
        }
    }

    fn fail_inbound_retirement(
        &self,
        completion: Option<InboundRetirementCompletion>,
        message: String,
    ) {
        let Some(completion) = completion else {
            return;
        };
        if let Some(request) = completion.request {
            let _ = request.fail_undelivered(Error::Verbs(std::io::Error::other(message)));
        }
        if completion.selected
            && let Some(listener) = completion.listener.upgrade()
            && listener.finish_selected_route(completion.route)
        {
            self.enqueue_listener_work(&listener);
        }
    }

    fn retire_outbound_connection_route(
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
                Ok(RouteRetirement::Complete {
                    completion: None,
                    reject: None,
                })
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

    fn retire_inbound_connection_route(
        &self,
        shared: &EngineShared,
        encoded: u64,
        connection: &Arc<ConnectionState>,
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
        let state = route.take_state_if(|state| state.references_connection(connection.token));
        match state {
            Some(InboundState::EstablishedAwaitingDelivery { request, .. }) => {
                if request.is_cancelled() && request.mark_cancellation_counted() {
                    shared
                        .diagnostic_counters
                        .accept_cancellations_after_selection
                        .fetch_add(1, Ordering::Relaxed);
                }
                let delivered = request.fail_undelivered(Error::DriverShutdown);
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
                if let Some(request) = request.as_ref()
                    && request.is_cancelled()
                    && request.mark_cancellation_counted()
                {
                    shared
                        .diagnostic_counters
                        .accept_cancellations_after_selection
                        .fetch_add(1, Ordering::Relaxed);
                }
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
            Some(state) => {
                route.set_state(state);
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

    fn lookup_dispatch_route(
        &self,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<CmDispatchRoute, CmEventReject> {
        if snapshot.event_type == CmEventType::ConnectRequest {
            let token = lock_unpoison(&self.listener_ids)
                .get(&snapshot.listen_id)
                .copied()
                .ok_or(CmEventReject::Unknown)?;
            let listener = lock_unpoison(&self.listeners)
                .get(&token)
                .cloned()
                .ok_or(CmEventReject::Stale)?;
            return Ok(CmDispatchRoute::Listener(listener));
        }
        if snapshot.context_key == 0 {
            return Err(CmEventReject::Unknown);
        }
        let route = lock_unpoison(&self.context_routes)
            .get(&snapshot.context_key)
            .copied()
            .ok_or(CmEventReject::Unknown)?;
        match route {
            ContextRoute::Outbound { .. } => self
                .lookup_event_route(snapshot)
                .map(CmDispatchRoute::Outbound),
            ContextRoute::Inbound { token, raw_id } => {
                if raw_id != snapshot.id {
                    return Err(CmEventReject::WrongId);
                }
                let route = match self.inbound_routes.lookup_cloned(token) {
                    Lookup::Occupied(route) => route,
                    Lookup::Duplicate => return Err(CmEventReject::Duplicate),
                    Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
                    Lookup::Unknown => return Err(CmEventReject::Unknown),
                };
                if route.raw_id.load(Ordering::Acquire) != raw_id {
                    return Err(CmEventReject::WrongId);
                }
                Ok(CmDispatchRoute::Inbound(route))
            }
            ContextRoute::Listener { token, raw_id } => {
                if raw_id != snapshot.id {
                    return Err(CmEventReject::WrongId);
                }
                let listener = lock_unpoison(&self.listeners)
                    .get(&token)
                    .cloned()
                    .ok_or(CmEventReject::Stale)?;
                Ok(CmDispatchRoute::Listener(listener))
            }
        }
    }

    fn lookup_event_route(
        &self,
        snapshot: CmEventSnapshot,
    ) -> std::result::Result<Arc<OutboundRoute>, CmEventReject> {
        if snapshot.context_key == 0 {
            return Err(CmEventReject::Unknown);
        }
        let route = lock_unpoison(&self.context_routes)
            .get(&snapshot.context_key)
            .copied()
            .ok_or(CmEventReject::Unknown)?;
        let ContextRoute::Outbound { token, raw_id } = route else {
            return Err(CmEventReject::Unexpected);
        };
        if raw_id != snapshot.id {
            return Err(CmEventReject::WrongId);
        }
        let route = match self.routes.lookup_cloned(token) {
            Lookup::Occupied(route) => route,
            Lookup::Duplicate => return Err(CmEventReject::Duplicate),
            Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
            Lookup::Unknown => return Err(CmEventReject::Unknown),
        };
        if route.raw_id.load(Ordering::Acquire) != raw_id {
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

    fn handle_inbound_event(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<InboundRoute>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        if is_failure_event(snapshot.event_type) || snapshot.status != 0 {
            return self.handle_inbound_failure(shared, route, snapshot);
        }
        match snapshot.event_type {
            CmEventType::Established => {
                let Some(InboundState::AwaitEstablished {
                    request,
                    connection,
                }) = route
                    .take_state_if(|state| matches!(state, InboundState::AwaitEstablished { .. }))
                else {
                    return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
                };
                let listener = route.listener.upgrade();
                if request.is_cancelled()
                    || listener
                        .as_ref()
                        .is_none_or(|listener| listener.is_closing())
                    || shared.shutdown_requested.load(Ordering::Acquire)
                {
                    let connection_state = Arc::clone(&connection.state);
                    let completion = if request.is_cancelled() {
                        None
                    } else if let Some(listener) = listener.as_ref() {
                        Some(listener.close_error())
                    } else {
                        Some(Error::DriverShutdown)
                    };
                    route.set_state(InboundState::Closing {
                        connection: EstablishedConnectionRoute::new(&connection_state),
                        request: Some(request),
                        completion,
                        selected: true,
                        reject: None,
                    });
                    shared.begin_connection_close(&connection_state);
                    drop(connection);
                    if connection_state.accepted_count() == 0 {
                        self.retire_registered_connection(shared, connection_state.token)?;
                    }
                    return Ok(EventDisposition::Handled);
                }

                let connection_route = EstablishedConnectionRoute::new(&connection.state);
                route.set_state(InboundState::EstablishedAwaitingDelivery {
                    request: Arc::clone(&request),
                    connection: connection_route,
                });
                shared
                    .diagnostic_counters
                    .connections_opened
                    .fetch_add(1, Ordering::Relaxed);
                shared
                    .diagnostic_counters
                    .inbound_requests_accepted
                    .fetch_add(1, Ordering::Relaxed);
                request.complete_success(connection);
                Ok(EventDisposition::Handled)
            }
            CmEventType::Disconnected => self.handle_inbound_disconnected(shared, route),
            CmEventType::TimewaitExit => Ok(EventDisposition::Handled),
            _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
        }
    }

    fn handle_inbound_disconnected(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<InboundRoute>,
    ) -> Result<EventDisposition> {
        let state = route.take_state_if(|state| {
            matches!(
                state,
                InboundState::AwaitEstablished { .. }
                    | InboundState::EstablishedAwaitingDelivery { .. }
                    | InboundState::Established { .. }
            )
        });
        let Some(state) = state else {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        };
        let (connection, request, selected) = match state {
            InboundState::AwaitEstablished {
                request,
                connection,
            } => (
                EstablishedConnectionRoute::new(&connection.state),
                Some(request),
                true,
            ),
            InboundState::EstablishedAwaitingDelivery {
                request,
                connection,
            } => (connection, Some(request), true),
            InboundState::Established { connection } => (connection, None, false),
            _ => unreachable!("inbound disconnect state was pre-filtered"),
        };
        let Some(connection_state) = connection.upgrade() else {
            if let Some(request) = request {
                let _ = request.fail_undelivered(Error::Verbs(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "inbound disconnect lost connection state before accept retirement",
                )));
            }
            if selected
                && let Some(listener) = route.listener.upgrade()
                && listener.finish_selected_route(route.token.encode())
            {
                self.enqueue_listener_work(&listener);
            }
            self.inbound_routes.release(route.token, true);
            return Ok(EventDisposition::Handled);
        };
        connection_state.mark_disconnected();
        route.set_state(InboundState::Closing {
            connection: connection.clone(),
            request: request.clone(),
            completion: request.as_ref().map(|_| {
                Error::Verbs(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "inbound connection disconnected during establishment",
                ))
            }),
            selected,
            reject: None,
        });
        if let Some(request) = request
            && request.fail_undelivered(Error::Verbs(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "inbound connection disconnected before accept delivery",
            )))
        {
            route.set_state(InboundState::Closing {
                connection: connection.clone(),
                request: None,
                completion: None,
                selected: false,
                reject: None,
            });
            if let Some(listener) = route.listener.upgrade()
                && listener.finish_selected_route(route.token.encode())
            {
                self.enqueue_listener_work(&listener);
            }
        }
        shared.begin_connection_close(&connection_state);
        if connection_state.accepted_count() == 0 {
            self.retire_registered_connection(shared, connection_state.token)?;
        }
        Ok(EventDisposition::Handled)
    }

    fn handle_inbound_failure(
        &self,
        shared: &Arc<EngineShared>,
        route: &Arc<InboundRoute>,
        snapshot: CmEventSnapshot,
    ) -> Result<EventDisposition> {
        let message = format!(
            "inbound RDMA CM {:?} failed with status {} for id={:#x} listen_id={:#x}",
            snapshot.event_type, snapshot.status, snapshot.id, snapshot.listen_id
        );
        let state = route.take_state_if(|_| true);
        let Some(state) = state else {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        };
        match state {
            InboundState::AwaitEstablished {
                request,
                connection,
            } => {
                let connection_state = Arc::clone(&connection.state);
                connection_state
                    .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())));
                route.set_state(InboundState::Closing {
                    connection: EstablishedConnectionRoute::new(&connection_state),
                    request: Some(request),
                    completion: Some(Error::Verbs(std::io::Error::other(message))),
                    selected: true,
                    reject: None,
                });
                shared.begin_connection_close(&connection_state);
                drop(connection);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            InboundState::EstablishedAwaitingDelivery {
                request,
                connection,
            } => {
                let Some(connection_state) = connection.upgrade() else {
                    self.inbound_routes.release(route.token, true);
                    return Ok(EventDisposition::Handled);
                };
                connection_state
                    .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())));
                route.set_state(InboundState::Closing {
                    connection: connection.clone(),
                    request: Some(Arc::clone(&request)),
                    completion: Some(Error::Verbs(std::io::Error::other(message.clone()))),
                    selected: true,
                    reject: None,
                });
                if request.fail_undelivered(Error::Verbs(std::io::Error::other(message))) {
                    route.set_state(InboundState::Closing {
                        connection: connection.clone(),
                        request: None,
                        completion: None,
                        selected: false,
                        reject: None,
                    });
                    if let Some(listener) = route.listener.upgrade()
                        && listener.finish_selected_route(route.token.encode())
                    {
                        self.enqueue_listener_work(&listener);
                    }
                }
                shared.begin_connection_close(&connection_state);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            InboundState::Established { connection } => {
                let Some(connection_state) = connection.upgrade() else {
                    self.inbound_routes.release(route.token, true);
                    return Ok(EventDisposition::Handled);
                };
                connection_state
                    .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())));
                route.set_state(InboundState::Closing {
                    connection: connection.clone(),
                    request: None,
                    completion: None,
                    selected: false,
                    reject: None,
                });
                shared.begin_connection_close(&connection_state);
                if connection_state.accepted_count() == 0 {
                    self.retire_registered_connection(shared, connection_state.token)?;
                }
            }
            state @ InboundState::Closing { .. } => {
                route.set_state(state);
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            InboundState::Transitioning => {
                return Err(Error::InvalidConfig(
                    "inbound CM route was re-entered while transitioning".into(),
                ));
            }
        }
        Ok(EventDisposition::Handled)
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
            Some(ConnectionCmRoute::Outbound(route.token.encode())),
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
        let establish = run_setup_before_establish(
            setup,
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
        shared.begin_connection_close(&connection_state);
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
        shared.begin_connection_close(&connection_state);
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
                connection_state
                    .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())));
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
                shared.begin_connection_close(&connection_state);
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
                connection_state
                    .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())));
                route.set_state(OutboundState::Failed {
                    connection: connection.clone(),
                });
                shared.begin_connection_close(&connection_state);
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
    let (admission, reservation) = reserve_connection(&shared)?;
    let request = Arc::new(OutboundRequest::new(address, config, setup, reservation));
    #[cfg(any(test, feature = "test-hooks"))]
    shared
        .test_driver
        .pause_admission(super::driver::test_api::AdmissionPausePoint::ConnectBeforeEnqueue);
    shared.cm.enqueue(Arc::clone(&request));
    drop(admission);
    shared.work_signal.publish(CM_WORK);
    ConnectWaiter {
        shared,
        request,
        finished: false,
    }
    .await
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

enum CmDispatchRoute {
    Outbound(Arc<OutboundRoute>),
    Inbound(Arc<InboundRoute>),
    Listener(Arc<ListenerState>),
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
    Transitioning,
}

impl InboundState {
    fn references_connection(&self, token: ConnectionToken) -> bool {
        match self {
            Self::AwaitEstablished { connection, .. } => connection.state.token == token,
            Self::EstablishedAwaitingDelivery { connection, .. }
            | Self::Established { connection }
            | Self::Closing { connection, .. } => connection.token == token,
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
        lock_unpoison(&cm.context_routes).insert(
            0x2000,
            ContextRoute::Outbound {
                token,
                raw_id: 0x1000,
            },
        );

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
            ContextRoute::Outbound {
                token: CmRouteToken {
                    slot: token.slot,
                    generation: token.generation + 1,
                },
                raw_id: 0x1000,
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
    fn inbound_routes_use_exact_generational_context_and_id_identity() {
        let cm = CmState::new(2).unwrap();
        let listener = ListenerState::test_only(2);
        let (token, route) = cm
            .inbound_routes
            .allocate_with(|token| Arc::new(InboundRoute::new(token, Arc::downgrade(&listener))))
            .unwrap();
        route.set_identity(0x4000, 0x5000);
        lock_unpoison(&cm.context_routes).insert(
            0x5000,
            ContextRoute::Inbound {
                token,
                raw_id: 0x4000,
            },
        );
        let exact = CmEventSnapshot {
            event_type: CmEventType::Established,
            status: 0,
            id: 0x4000,
            listen_id: 0,
            context_key: 0x5000,
        };
        assert!(matches!(
            cm.lookup_dispatch_route(exact),
            Ok(CmDispatchRoute::Inbound(_))
        ));
        assert!(matches!(
            cm.lookup_dispatch_route(CmEventSnapshot {
                id: 0x4001,
                ..exact
            }),
            Err(CmEventReject::WrongId)
        ));
        cm.inbound_routes.release(token, true);
        assert!(matches!(
            cm.lookup_dispatch_route(exact),
            Err(CmEventReject::Duplicate)
        ));
        assert!(!cm.remove_context_route_if_owned(0x5000, 0x4001, Some(token.encode())));
        assert!(
            !cm.remove_context_route_if_owned(
                0x5000,
                0x4000,
                Some(
                    CmRouteToken {
                        slot: token.slot,
                        generation: token.generation + 1,
                    }
                    .encode()
                )
            )
        );
        assert!(cm.remove_context_route_if_owned(0x5000, 0x4000, Some(token.encode())));
    }

    #[test]
    fn unowned_child_context_never_removes_listener_event_route() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let listener = ListenerState::test_only(2);
        let listener_token = 17;
        lock_unpoison(&engine.shared.cm.listeners).insert(listener_token, Arc::clone(&listener));
        lock_unpoison(&engine.shared.cm.context_routes).insert(
            0x5000,
            ContextRoute::Listener {
                token: listener_token,
                raw_id: 0x4000,
            },
        );

        assert!(
            !engine
                .shared
                .cm
                .remove_context_route_if_owned(0x5000, 0x4001, None)
        );
        assert!(matches!(
            lock_unpoison(&engine.shared.cm.context_routes)
                .get(&0x5000)
                .copied(),
            Some(ContextRoute::Listener {
                token: 17,
                raw_id: 0x4000
            })
        ));

        let snapshot = CmEventSnapshot {
            event_type: CmEventType::AddrChange,
            status: libc::EADDRNOTAVAIL,
            id: 0x4000,
            listen_id: 0,
            context_key: 0x5000,
        };
        let routed = engine.shared.cm.lookup_dispatch_route(snapshot).unwrap();
        let CmDispatchRoute::Listener(routed_listener) = routed else {
            panic!("listener context route changed after unowned child rejection");
        };
        assert!(Arc::ptr_eq(&routed_listener, &listener));
        assert!(matches!(
            engine
                .shared
                .cm
                .handle_listener_event(&engine.shared, &listener, snapshot),
            Ok(EventDisposition::Handled)
        ));
        assert!(listener.is_closing());
        assert!(matches!(listener.close_error(), Error::Verbs(_)));

        let removed = CmEventSnapshot {
            event_type: CmEventType::DeviceRemoval,
            ..snapshot
        };
        assert!(matches!(
            engine
                .shared
                .cm
                .handle_listener_event(&engine.shared, &listener, removed),
            Err(Error::Verbs(_))
        ));

        let wrong_generation = CmRouteToken {
            slot: 0,
            generation: 2,
        }
        .encode();
        assert!(!engine.shared.cm.remove_context_route_if_owned(
            0x5000,
            0x4000,
            Some(wrong_generation)
        ));
        assert!(engine.shared.cm.remove_context_route_if_owned(
            0x5000,
            0x4000,
            Some(listener_token)
        ));
        drop(driver);
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

        assert!(cm.insert_context_route(
            0x2000,
            ContextRoute::Outbound {
                token: incumbent,
                raw_id: 0x1000,
            }
        ));
        assert!(!cm.insert_context_route(
            0x2000,
            ContextRoute::Outbound {
                token: duplicate,
                raw_id: 0x1001,
            }
        ));
        assert_eq!(
            lock_unpoison(&cm.context_routes).get(&0x2000).copied(),
            Some(ContextRoute::Outbound {
                token: incumbent,
                raw_id: 0x1000,
            })
        );
    }

    #[test]
    fn duplicate_listener_identity_keeps_both_incumbent_mappings() {
        let cm = CmState::new(2).unwrap();
        let incumbent = ListenerState::test_only(1);
        let duplicate_token = ListenerState::test_only(1);
        let duplicate_id = ListenerState::test_only(1);

        assert!(cm.insert_listener_identity(7, 0x1000, Arc::clone(&incumbent)));
        assert!(!cm.insert_listener_identity(7, 0x2000, duplicate_token));
        assert!(!cm.insert_listener_identity(8, 0x1000, duplicate_id));
        assert!(
            lock_unpoison(&cm.listeners)
                .get(&7)
                .is_some_and(|listener| Arc::ptr_eq(listener, &incumbent))
        );
        assert_eq!(lock_unpoison(&cm.listeners).len(), 1);
        assert_eq!(
            lock_unpoison(&cm.listener_ids).get(&0x1000).copied(),
            Some(7)
        );
        assert_eq!(lock_unpoison(&cm.listener_ids).len(), 1);
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
        let summary = run_setup_before_establish(
            Box::new(RecordingSetup {
                order: Arc::clone(&order),
                result: Ok(SetupSummary { posted_wrs: 0 }),
            }),
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
        let error = run_setup_before_establish(
            Box::new(RecordingSetup {
                order: Arc::clone(&order),
                result: Err(Error::InvalidConfig("setup failed".into())),
            }),
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
        let error = run_setup_before_establish(
            Box::new(RecordingSetup {
                order: Arc::clone(&order),
                result: Ok(SetupSummary { posted_wrs: 1 }),
            }),
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

        engine.shared.cm.begin_shutdown(
            &engine.shared,
            &EngineOutcome::Failure(super::super::EngineFailure::DriverShutdown),
        );

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
    fn transitioning_route_requeues_retirement_once_per_service_pass() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let request = Arc::new(test_request());
        let (route_token, route) = engine
            .shared
            .cm
            .routes
            .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
            .unwrap();
        let (admission, reservation) = reserve_connection(&engine.shared).unwrap();
        let connection = install_reserved_connection(
            &engine.shared,
            Arc::new(NoopPoster(13)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
            reservation,
            Some(ConnectionCmRoute::Outbound(route_token.encode())),
        )
        .unwrap();
        drop(admission);

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
        assert!(connection.state.transition_to_error_once().unwrap());
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
    fn inbound_disconnect_without_connection_state_fails_and_retires_selected_accept() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let listener = ListenerState::test_only(1);
        let (route_token, route) = engine
            .shared
            .cm
            .inbound_routes
            .allocate_with(|token| Arc::new(InboundRoute::new(token, Arc::downgrade(&listener))))
            .unwrap();
        let request = selected_accept(&listener, route_token.encode());
        let connection = Arc::new(ConnectionState::new(
            ConnectionToken {
                slot: 9,
                generation: 3,
            },
            Arc::new(NoopPoster(31)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
            None,
            None,
        ));
        route.set_state(InboundState::EstablishedAwaitingDelivery {
            request: Arc::clone(&request),
            connection: EstablishedConnectionRoute::new(&connection),
        });
        drop(connection);

        assert!(matches!(
            engine
                .shared
                .cm
                .handle_inbound_disconnected(&engine.shared, &route),
            Ok(EventDisposition::Handled)
        ));
        let Some(Err(error)) = request.take_result_for_test() else {
            panic!("selected accept must fail");
        };
        assert!(
            error
                .to_string()
                .contains("lost connection state before accept retirement")
        );
        assert_eq!(listener.queue_counts().2, 0);
        assert!(matches!(
            engine.shared.cm.inbound_routes.lookup_cloned(route_token),
            Lookup::Duplicate
        ));

        drop(engine);
        drop(driver);
    }

    #[test]
    fn listener_destroy_error_completes_close_once_before_propagation() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let listener_state = ListenerState::test_only(1);
        let listener = RdmaListener {
            shared: Arc::clone(&engine.shared),
            state: Arc::clone(&listener_state),
        };
        let mut close = Box::pin(listener.close());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(close.as_mut().poll(&mut cx).is_pending());
        let destroy_count = Arc::new(AtomicUsize::new(0));
        lock_unpoison(&engine.shared.cm.cm_destructions).push_back(PendingCmDestruction::Test {
            destroy_count: Arc::clone(&destroy_count),
            target: TestCmDestruction::Listener {
                listener: listener_state,
                destroy_error: Some(
                    "destroy listener CM ID for 127.0.0.1:1: injected failure".into(),
                ),
            },
        });

        let error = engine
            .shared
            .cm
            .service_cm_destructions(&engine.shared, 1, || Ok(false))
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        let Poll::Ready(Err(close_error)) = close.as_mut().poll(&mut cx) else {
            panic!("listener close was not completed before destroy failure propagation");
        };
        assert_eq!(close_error.to_string(), error.to_string());
        assert_eq!(destroy_count.load(Ordering::Acquire), 1);
        assert!(lock_unpoison(&engine.shared.cm.cm_destructions).is_empty());
        assert_eq!(
            engine
                .shared
                .cm
                .service_cm_destructions(&engine.shared, 1, || Ok(false))
                .unwrap(),
            0
        );
        assert_eq!(destroy_count.load(Ordering::Acquire), 1);

        drop(close);
        drop(listener);
        drop(engine);
        drop(driver);
    }

    #[test]
    fn connection_destroy_error_fails_accept_and_retirement_once() {
        assert_connection_cm_destruction_failure(
            Some("destroy connection CM ID: injected destroy failure"),
            None,
            "injected destroy failure",
            false,
        );
    }

    #[test]
    fn connection_finalize_error_fails_accept_and_retirement_once() {
        assert_connection_cm_destruction_failure(
            None,
            Some("injected finalization failure"),
            "finalize connection retirement after CM destruction",
            true,
        );
    }

    #[test]
    fn cm_destroy_barrier_is_budgeted_across_service_passes() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let listener_state = ListenerState::test_only(1);
        let listener = RdmaListener {
            shared: Arc::clone(&engine.shared),
            state: Arc::clone(&listener_state),
        };
        let mut close = Box::pin(listener.close());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(close.as_mut().poll(&mut cx).is_pending());
        let destroy_count = Arc::new(AtomicUsize::new(0));
        lock_unpoison(&engine.shared.cm.cm_destructions).push_back(PendingCmDestruction::Test {
            destroy_count: Arc::clone(&destroy_count),
            target: TestCmDestruction::Listener {
                listener: listener_state,
                destroy_error: None,
            },
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
            assert_eq!(destroy_count.load(Ordering::Acquire), 0);
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
        assert_eq!(destroy_count.load(Ordering::Acquire), 1);
        assert!(lock_unpoison(&engine.shared.cm.cm_destructions).is_empty());
        assert!(matches!(close.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        listener
            .state
            .finish_close(Some(Error::InvalidConfig("late duplicate finish".into())));
        let mut repeated_close = Box::pin(listener.close());
        assert!(matches!(
            repeated_close.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(
            engine
                .shared
                .cm
                .service_cm_destructions(&engine.shared, 1, || Ok(false))
                .unwrap(),
            0
        );
        assert_eq!(destroy_count.load(Ordering::Acquire), 1);
        drop(engine);
        drop(driver);
    }

    fn selected_accept(listener: &Arc<ListenerState>, route: u64) -> Arc<AcceptRequest> {
        let request = AcceptRequest::test_only();
        listener.register_waiter(Arc::clone(&request)).unwrap();
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        let ListenerAction::ProcessSelected {
            request: selected, ..
        } = listener.next_action()
        else {
            panic!("accept was not selected");
        };
        assert!(Arc::ptr_eq(&selected, &request));
        listener.route_selected(&request, route).unwrap();
        request
    }

    fn assert_connection_cm_destruction_failure(
        destroy_error: Option<&str>,
        finalize_error: Option<&str>,
        expected: &str,
        registry_retained: bool,
    ) {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let connection = install_connection(
            &engine.shared,
            Arc::new(NoopPoster(32)),
            RdmaConnectionConfig::default()
                .max_send_wr(1)
                .max_recv_wr(1),
            None,
            None,
        )
        .unwrap();
        let listener = ListenerState::test_only(1);
        let route = 0x0000_0001_0000_0001;
        let request = selected_accept(&listener, route);
        let completion = InboundRetirementCompletion {
            listener: Arc::downgrade(&listener),
            route,
            request: Some(Arc::clone(&request)),
            result: Some(Error::TransportClosed),
            selected: true,
        };
        let destroy_count = Arc::new(AtomicUsize::new(0));
        lock_unpoison(&engine.shared.cm.cm_destructions).push_back(PendingCmDestruction::Test {
            destroy_count: Arc::clone(&destroy_count),
            target: TestCmDestruction::Connection {
                connection: Arc::clone(&connection.state),
                completion: Some(completion),
                destroy_error: destroy_error.map(str::to_owned),
                finalize_error: finalize_error.map(str::to_owned),
            },
        });

        let error = engine
            .shared
            .cm
            .service_cm_destructions(&engine.shared, 1, || Ok(false))
            .unwrap_err();
        assert!(error.to_string().contains(expected));
        assert!(connection.state.is_retired());
        let Some(Err(request_error)) = request.take_result_for_test() else {
            panic!("inbound accept must fail before propagation");
        };
        assert!(request_error.to_string().contains(expected));
        assert_eq!(listener.queue_counts().2, 0);
        let mut close = Box::pin(connection.close());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let Poll::Ready(Err(close_error)) = close.as_mut().poll(&mut cx) else {
            panic!("connection retirement was not completed before propagation");
        };
        assert!(close_error.to_string().contains(expected));
        assert_eq!(destroy_count.load(Ordering::Acquire), 1);
        assert!(lock_unpoison(&engine.shared.cm.cm_destructions).is_empty());
        assert_eq!(
            engine
                .shared
                .cm
                .service_cm_destructions(&engine.shared, 1, || Ok(false))
                .unwrap(),
            0
        );
        assert_eq!(destroy_count.load(Ordering::Acquire), 1);

        let retained = engine
            .shared
            .connections
            .release(connection.state.token, connection.state.qp_num());
        assert_eq!(retained.is_some(), registry_retained);
        drop(close);
        drop(connection);
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
        fn run(self: Box<Self>, _connection: &RdmaConnection) -> Result<SetupSummary> {
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
