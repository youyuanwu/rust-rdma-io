//! Shared RDMA-CM routing and outbound connection state machines.
//!
//! The engine driver is the only consumer of the shared CM event channel.
//! Every outbound ID owns an opaque context allocation indexed to a
//! non-wrapping route token. Events are copied into an identity snapshot and
//! acknowledged before state ownership advances or any potentially blocking
//! librdmacm/verbs call runs.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::connection::{
    ConnectionReservation, VerbsConnectionResources, WorkRequestPoster,
    install_reserved_connection, reserve_connection,
};
use super::diagnostics::CmEventReject;
use super::registry::{Lookup, PagedRegistry, RegistryToken, lock_unpoison};
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
    shutting_down: AtomicBool,
}

impl CmState {
    pub(super) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            routes: PagedRegistry::new(capacity)?,
            context_routes: Mutex::new(HashMap::new()),
            pending: Mutex::new(VecDeque::new()),
            cancellations: Mutex::new(VecDeque::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn enqueue(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.pending).push_back(request);
    }

    fn enqueue_cancellation(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.cancellations).push_back(request);
    }

    pub(super) fn has_software_work(&self) -> bool {
        !lock_unpoison(&self.pending).is_empty() || !lock_unpoison(&self.cancellations).is_empty()
    }

    pub(super) fn service_software(
        &self,
        shared: &Arc<EngineShared>,
        resources: &EngineResources,
        budget: usize,
    ) -> Result<usize> {
        let mut processed = 0;
        while processed < budget {
            if let Some(request) = lock_unpoison(&self.cancellations).pop_front() {
                self.process_cancellation(shared, request)?;
                processed += 1;
                continue;
            }
            let Some(request) = lock_unpoison(&self.pending).pop_front() else {
                break;
            };
            self.start_outbound(shared, resources, request)?;
            processed += 1;
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
        let requests: Vec<_> = self
            .routes
            .occupied_cloned()
            .into_iter()
            .filter_map(|route| route.request())
            .chain(pending)
            .collect();
        for request in requests {
            request.cancelled.store(true, Ordering::Release);
            drop(request.take_reservation());
            request.complete(match outcome.clone().into_result() {
                Err(error) => Err(error),
                Ok(()) => unreachable!("successful engine outcome was filtered"),
            });
        }
    }

    pub(super) fn pending_route_count(&self) -> usize {
        self.routes
            .occupied_cloned()
            .into_iter()
            .filter(|route| route.is_establishing())
            .count()
    }

    pub(super) fn route_count(&self) -> usize {
        self.routes.live()
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
            request.complete(match outcome.clone().into_result() {
                Err(error) => Err(error),
                Ok(()) => unreachable!("successful engine outcome was filtered"),
            });
        }
    }

    fn start_outbound(
        &self,
        _shared: &Arc<EngineShared>,
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
                request.complete(Err(error));
                return Ok(());
            }
        };
        request.route_token.store(token.encode(), Ordering::Release);

        let cm_id = match CmId::new_with_context_token(
            &resources.cm_event_channel,
            PortSpace::Tcp,
            token.encode(),
        ) {
            Ok(cm_id) => cm_id,
            Err(error) => {
                self.routes.release(token, false);
                drop(reservation);
                request.complete(Err(error.into()));
                return Ok(());
            }
        };
        let context_key = cm_id.context_key();
        route.set_identity(cm_id.as_raw() as usize, context_key);
        {
            let mut contexts = lock_unpoison(&self.context_routes);
            if contexts.insert(context_key, token).is_some() {
                drop(contexts);
                drop(cm_id);
                self.routes.release(token, false);
                drop(reservation);
                request.complete(Err(Error::InvalidConfig(
                    "duplicate CM context identity".into(),
                )));
                return Ok(());
            }
        }

        let resolve = cm_id.resolve_addr(None, &request.address, 2_000);
        match resolve {
            Ok(()) => route.set_state(OutboundState::AwaitAddr {
                cm_id,
                request,
                reservation,
            }),
            Err(error) => {
                drop(cm_id);
                self.retire_route(&route, false);
                drop(reservation);
                request.complete(Err(error.into()));
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
                OutboundState::EstablishedPendingDelivery { .. }
                    | OutboundState::Disconnected { .. }
                    | OutboundState::FailedEstablished { .. }
            )
        });
        let Some(state) = state else {
            return Ok(());
        };
        let (request, connection) = match state {
            OutboundState::EstablishedPendingDelivery {
                request,
                connection,
            } => (request, connection),
            OutboundState::Disconnected {
                request,
                connection,
            }
            | OutboundState::FailedEstablished {
                request,
                connection,
            } => {
                let Some(connection) = connection.upgrade() else {
                    self.retire_route(&route, true);
                    return Ok(());
                };
                (
                    request,
                    RdmaConnection {
                        shared: Arc::clone(shared),
                        state: connection,
                    },
                )
            }
            _ => unreachable!("cancellation state was pre-filtered"),
        };
        let destroyed = cleanup_registered_connection(shared, &connection);
        if destroyed {
            drop(connection);
            self.retire_route(&route, true);
        } else {
            route.set_state(OutboundState::RetainedFailure {
                request,
                connection,
            });
        }
        Ok(())
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
            CmEventType::Disconnected => self.handle_disconnected(route),
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
            drop(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete(Err(Error::DriverShutdown));
            return Ok(EventDisposition::Handled);
        }
        if let Err(error) = cm_id.require_context(resources.context.inner()) {
            drop(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete(Err(error.into()));
            return Ok(EventDisposition::Handled);
        }
        match cm_id.resolve_route(2_000) {
            Ok(()) => route.set_state(OutboundState::AwaitRoute {
                cm_id,
                request,
                reservation,
            }),
            Err(error) => {
                drop(cm_id);
                drop(reservation);
                self.retire_route(route, true);
                request.complete(Err(error.into()));
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
            drop(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete(Err(Error::DriverShutdown));
            return Ok(EventDisposition::Handled);
        }
        if let Err(error) = cm_id.require_context(resources.context.inner()) {
            drop(cm_id);
            drop(reservation);
            self.retire_route(route, true);
            request.complete(Err(error.into()));
            return Ok(EventDisposition::Handled);
        }

        let local_addr = cm_id.local_addr();
        let peer_addr = cm_id.peer_addr();
        let qp = match build_qp(resources, &cm_id, &request.config) {
            Ok(qp) => qp,
            Err(error) => {
                drop(cm_id);
                drop(reservation);
                self.retire_route(route, true);
                request.complete(Err(error));
                return Ok(EventDisposition::Handled);
            }
        };
        let verbs = Arc::new(VerbsConnectionResources::new_shared(
            qp,
            cm_id,
            Arc::clone(&resources.cm_event_channel),
        ));
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
                verbs.destroy_qp();
                drop(verbs);
                self.retire_route(route, true);
                request.complete(Err(error));
                return Ok(EventDisposition::Handled);
            }
        };

        let Some(setup) = request.take_setup() else {
            let destroyed = cleanup_registered_connection(shared, &connection);
            drop(verbs);
            if destroyed {
                drop(connection);
                self.retire_route(route, true);
                request.complete(Err(Error::InvalidConfig(
                    "outbound request setup was consumed more than once".into(),
                )));
            } else {
                let waiter = Arc::clone(&request);
                route.set_state(OutboundState::RetainedFailure {
                    request,
                    connection,
                });
                waiter.complete(Err(Error::InvalidConfig(
                    "outbound request setup was consumed more than once".into(),
                )));
            }
            return Ok(EventDisposition::Handled);
        };
        let conn_param = match request.config.conn_param() {
            Ok(param) => param,
            Err(error) => {
                let destroyed = cleanup_registered_connection(shared, &connection);
                drop(verbs);
                if destroyed {
                    drop(connection);
                    self.retire_route(route, true);
                    request.complete(Err(error));
                } else {
                    let waiter = Arc::clone(&request);
                    route.set_state(OutboundState::RetainedFailure {
                        request,
                        connection,
                    });
                    waiter.complete(Err(error));
                }
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
            let destroyed = cleanup_registered_connection(shared, &connection);
            drop(verbs);
            if destroyed {
                drop(connection);
                self.retire_route(route, true);
                request.complete(Err(error));
            } else {
                let waiter = Arc::clone(&request);
                route.set_state(OutboundState::RetainedFailure {
                    request,
                    connection,
                });
                waiter.complete(Err(error));
            }
            return Ok(EventDisposition::Handled);
        }
        if request.cancelled.load(Ordering::Acquire)
            || shared.shutdown_requested.load(Ordering::Acquire)
        {
            let destroyed = cleanup_registered_connection(shared, &connection);
            drop(verbs);
            if destroyed {
                drop(connection);
                self.retire_route(route, true);
                request.complete(Err(Error::DriverShutdown));
            } else {
                let waiter = Arc::clone(&request);
                route.set_state(OutboundState::RetainedFailure {
                    request,
                    connection,
                });
                waiter.complete(Err(Error::DriverShutdown));
            }
            return Ok(EventDisposition::Handled);
        }
        drop(verbs);
        route.set_state(OutboundState::AwaitEstablished {
            request,
            connection,
        });
        Ok(EventDisposition::Handled)
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
            let destroyed = cleanup_registered_connection(shared, &connection);
            if destroyed {
                drop(connection);
                self.retire_route(route, true);
                request.complete(Err(Error::DriverShutdown));
            } else {
                let waiter = Arc::clone(&request);
                route.set_state(OutboundState::RetainedFailure {
                    request,
                    connection,
                });
                waiter.complete(Err(Error::DriverShutdown));
            }
            return Ok(EventDisposition::Handled);
        }
        let result = connection.clone();
        let waiter = Arc::clone(&request);
        route.set_state(OutboundState::EstablishedPendingDelivery {
            request,
            connection,
        });
        shared
            .diagnostic_counters
            .connections_opened
            .fetch_add(1, Ordering::Relaxed);
        waiter.complete(Ok(result));
        Ok(EventDisposition::Handled)
    }

    fn handle_disconnected(&self, route: &Arc<OutboundRoute>) -> Result<EventDisposition> {
        let state = route.take_state_if(|state| {
            matches!(state, OutboundState::EstablishedPendingDelivery { .. })
        });
        let Some(OutboundState::EstablishedPendingDelivery {
            request,
            connection,
        }) = state
        else {
            if route.is_disconnected() {
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
        };
        connection.state.mark_disconnected();
        route.set_state(OutboundState::Disconnected {
            request,
            connection: Arc::downgrade(&connection.state),
        });
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
                drop(cm_id);
                drop(reservation);
                self.retire_route(route, true);
                request.complete(Err(Error::Verbs(std::io::Error::other(message))));
            }
            OutboundState::AwaitEstablished {
                request,
                connection,
            } => {
                let destroyed = cleanup_registered_connection(shared, &connection);
                if destroyed {
                    drop(connection);
                    self.retire_route(route, true);
                    request.complete(Err(Error::Verbs(std::io::Error::other(message))));
                } else {
                    let waiter = Arc::clone(&request);
                    route.set_state(OutboundState::RetainedFailure {
                        request,
                        connection,
                    });
                    waiter.complete(Err(Error::Verbs(std::io::Error::other(message))));
                }
            }
            OutboundState::EstablishedPendingDelivery {
                request,
                connection,
            } => {
                connection.state.mark_cm_failure(message);
                route.set_state(OutboundState::FailedEstablished {
                    request,
                    connection: Arc::downgrade(&connection.state),
                });
            }
            OutboundState::Disconnected {
                request,
                connection,
            }
            | OutboundState::FailedEstablished {
                request,
                connection,
            } => {
                route.set_state(OutboundState::FailedEstablished {
                    request,
                    connection,
                });
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            OutboundState::RetainedFailure {
                request,
                connection,
            } => {
                route.set_state(OutboundState::RetainedFailure {
                    request,
                    connection,
                });
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            }
            OutboundState::Transitioning => {
                return Err(Error::InvalidConfig(
                    "CM route was re-entered while transitioning".into(),
                ));
            }
        }
        shared
            .diagnostic_counters
            .connections_failed
            .fetch_add(1, Ordering::Relaxed);
        Ok(EventDisposition::Handled)
    }

    fn retire_route(&self, route: &Arc<OutboundRoute>, completed: bool) {
        let context_key = route.context_key.load(Ordering::Acquire);
        let mut contexts = lock_unpoison(&self.context_routes);
        if contexts.get(&context_key).copied() == Some(route.token) {
            contexts.remove(&context_key);
        }
        drop(contexts);
        self.routes.release(route.token, completed);
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
        delivered: false,
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
    let summary = setup.run(shared, connection)?;
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

fn cleanup_registered_connection(shared: &Arc<EngineShared>, connection: &RdmaConnection) -> bool {
    connection.state.stop_posting();
    let _ = connection.state.transition_to_error_once();
    if connection.state.accepted_count() != 0 {
        return false;
    }
    connection.state.destroy_qp_zero_outstanding();
    shared
        .connections
        .release(connection.state.token, connection.state.qp_num());
    true
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
            | OutboundState::EstablishedPendingDelivery { request, .. }
            | OutboundState::Disconnected { request, .. }
            | OutboundState::FailedEstablished { request, .. }
            | OutboundState::RetainedFailure { request, .. } => Some(Arc::clone(request)),
            OutboundState::Transitioning => None,
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
            OutboundState::Disconnected { .. } | OutboundState::FailedEstablished { .. }
        )
    }
}

enum OutboundState {
    AwaitAddr {
        cm_id: CmId,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
    },
    AwaitRoute {
        cm_id: CmId,
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
    },
    AwaitEstablished {
        request: Arc<OutboundRequest>,
        connection: RdmaConnection,
    },
    EstablishedPendingDelivery {
        request: Arc<OutboundRequest>,
        connection: RdmaConnection,
    },
    Disconnected {
        request: Arc<OutboundRequest>,
        #[allow(dead_code, reason = "used by Phase 6 route retirement")]
        connection: Weak<super::connection::ConnectionState>,
    },
    FailedEstablished {
        request: Arc<OutboundRequest>,
        #[allow(dead_code, reason = "used by Phase 6 route retirement")]
        connection: Weak<super::connection::ConnectionState>,
    },
    RetainedFailure {
        request: Arc<OutboundRequest>,
        connection: RdmaConnection,
    },
    Transitioning,
}

struct OutboundRequest {
    address: SocketAddr,
    config: RdmaConnectionConfig,
    setup: Mutex<Option<Box<dyn PreEstablishSetup>>>,
    reservation: Mutex<Option<ConnectionReservation>>,
    result: Mutex<Option<Result<RdmaConnection>>>,
    result_set: AtomicBool,
    cancelled: AtomicBool,
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
            result: Mutex::new(None),
            result_set: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
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
        if self.result_set.swap(true, Ordering::AcqRel) {
            return;
        }
        *lock_unpoison(&self.result) = Some(result);
        self.waker.wake();
    }

    fn take_result(&self) -> Option<Result<RdmaConnection>> {
        lock_unpoison(&self.result).take()
    }
}

struct ConnectWaiter {
    shared: Arc<EngineShared>,
    request: Arc<OutboundRequest>,
    delivered: bool,
}

impl Future for ConnectWaiter {
    type Output = Result<RdmaConnection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.request.take_result() {
            self.delivered = true;
            return Poll::Ready(result);
        }
        self.request.waker.register(cx.waker());
        if let Some(result) = self.request.take_result() {
            self.delivered = true;
            return Poll::Ready(result);
        }
        Poll::Pending
    }
}

impl Drop for ConnectWaiter {
    fn drop(&mut self) {
        if self.delivered || self.request.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
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

        cm.routes.release(token, true);
        assert!(matches!(
            cm.lookup_event_route(exact),
            Err(CmEventReject::Duplicate)
        ));
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
