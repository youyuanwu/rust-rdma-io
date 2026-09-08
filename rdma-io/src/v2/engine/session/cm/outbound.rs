//! Outbound connection setup, delivery, cancellation, and CM transitions.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;

use super::{
    CmEventReject, CmEventSnapshot, CmRouteToken, CmState, ConnectionCmRoute,
    ConnectionReservation, ConnectionSetup, ContextRoute, EngineResources,
    EstablishedConnectionRoute, EventDisposition, Lookup, OutboundRoute, OutboundState,
    RdmaConnection, RdmaConnectionConfig, SessionManager, SharedCmId, TakeOnceResult,
    VerbsConnectionResources, build_qp, empty_connection_setup, install_reserved_connection,
    is_failure_event, lock_unpoison, reserve_connection, run_setup_before_establish,
};
use crate::cm::{CmEventType, CmId, PortSpace};
use crate::v2::error::{Error, Result};

pub(in crate::v2::engine) async fn connect(
    shared: Arc<SessionManager>,
    address: SocketAddr,
    config: RdmaConnectionConfig,
) -> Result<RdmaConnection> {
    connect_with_setup(shared, address, config, empty_connection_setup()).await
}

pub(in crate::v2::engine) async fn connect_with_setup(
    shared: Arc<SessionManager>,
    address: SocketAddr,
    config: RdmaConnectionConfig,
    setup: ConnectionSetup,
) -> Result<RdmaConnection> {
    shared.validate_connection_config(&config)?;
    let (admission, reservation) = reserve_connection(&shared)?;
    let request = Arc::new(OutboundRequest::new(address, config, setup, reservation));
    #[cfg(any(test, feature = "test-hooks"))]
    shared.pause_connect_before_enqueue();
    shared.cm.enqueue(Arc::clone(&request));
    drop(admission);
    shared.publish_session_work();
    let waiter = ConnectWaiter {
        manager: Arc::downgrade(&shared),
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    };
    drop(request);
    drop(shared);
    waiter.await
}

pub(super) fn start(
    state: &CmState,
    resources: &EngineResources,
    request: Arc<OutboundRequest>,
) -> Result<bool> {
    if request.observer.cancelled.load(Ordering::Acquire)
        || state.shutting_down.load(Ordering::Acquire)
    {
        request.take_reservation();
        request.complete(Err(Error::DriverShutdown));
        return Ok(false);
    }
    let reservation = request.take_reservation().ok_or_else(|| {
        Error::InvalidConfig("outbound request lost its connection reservation".into())
    })?;
    let (token, route) = match state
        .routes
        .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
    {
        Ok(route) => route,
        Err(error) => {
            drop(reservation);
            request.complete_failure(error);
            return Ok(false);
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
            state.routes.release(token, false);
            drop(reservation);
            request.complete_failure(Error::from_v1(error));
            return Ok(false);
        }
    };
    let Some(context_token) = cm_id.context_token() else {
        state.defer_cm_id(cm_id);
        state.routes.release(token, false);
        drop(reservation);
        request.complete_failure(Error::InvalidConfig(
            "engine CM ID lost its route context token".into(),
        ));
        return Ok(false);
    };
    let context_route = CmRouteToken::decode(context_token);
    if context_route != token {
        state.defer_cm_id(cm_id);
        state.routes.release(token, false);
        drop(reservation);
        request.complete_failure(Error::InvalidConfig(
            "engine CM context token did not match its route".into(),
        ));
        return Ok(false);
    }
    let context_key = cm_id.context_key();
    route.set_identity(cm_id.as_raw() as usize, context_key);
    let raw_id = cm_id.as_raw() as usize;
    if !state.insert_context_route(
        context_key,
        ContextRoute::Outbound {
            token: context_route,
            raw_id,
        },
    ) {
        state.defer_cm_id(cm_id);
        state.routes.release(token, false);
        drop(reservation);
        request.complete_failure(Error::InvalidConfig("duplicate CM context identity".into()));
        return Ok(false);
    }

    let resolve = cm_id.resolve_addr(None, &request.address, 2_000);
    match resolve {
        Ok(()) => route.set_state(OutboundState::AwaitAddr {
            cm_id,
            request,
            reservation,
        }),
        Err(error) => {
            state.defer_cm_id(cm_id);
            state.retire_route(&route, false);
            drop(reservation);
            request.complete_failure(Error::from_v1(error));
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn process_cancellation(
    state: &CmState,
    shared: &SessionManager,
    request: Arc<OutboundRequest>,
) -> Result<()> {
    let encoded = request.route_token.load(Ordering::Acquire);
    if encoded == 0 {
        return Ok(());
    }
    let token = CmRouteToken::decode(encoded);
    let Lookup::Occupied(route) = state.routes.lookup_cloned(token) else {
        return Ok(());
    };
    let route_state = route.take_state_if(|route_state| {
        matches!(
            route_state,
            OutboundState::EstablishedAwaitingDelivery { .. }
                | OutboundState::DisconnectedAwaitingDelivery { .. }
                | OutboundState::FailedAwaitingDelivery { .. }
        )
    });
    let Some(route_state) = route_state else {
        return Ok(());
    };
    let (route_request, connection) = match route_state {
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
        state.retire_route(&route, true);
        return Ok(());
    };
    route.set_state(OutboundState::Closing {
        connection: connection.clone(),
    });
    shared.begin_connection_close(&connection_state);
    drop(request.take_result());
    if connection_state.accepted_count() == 0 {
        shared.retire_registered_connection(connection_state.token)?;
    }
    drop(route_request);
    Ok(())
}

pub(super) fn handle_event(
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    route: &Arc<OutboundRoute>,
    snapshot: CmEventSnapshot,
) -> Result<EventDisposition> {
    let disposition = if is_failure_event(snapshot.event_type) || snapshot.status != 0 {
        handle_failure_event(state, shared, route, snapshot)?
    } else {
        match snapshot.event_type {
            CmEventType::AddrResolved => handle_addr_resolved(state, shared, resources, route),
            CmEventType::RouteResolved => handle_route_resolved(state, shared, resources, route),
            CmEventType::Established => handle_established(state, shared, route),
            CmEventType::Disconnected => handle_disconnected(state, shared, route),
            CmEventType::TimewaitExit => {
                if route.is_disconnected() {
                    Ok(EventDisposition::Handled)
                } else {
                    Ok(EventDisposition::Rejected(CmEventReject::Unexpected))
                }
            }
            _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
        }?
    };
    let route_retired = !matches!(state.routes.lookup_cloned(route.token), Lookup::Occupied(_));
    if is_failure_event(snapshot.event_type)
        || snapshot.status != 0
        || snapshot.event_type == CmEventType::RouteResolved
        || snapshot.event_type == CmEventType::Established
        || route_retired
        || !route.is_establishing()
    {
        state.outbound_setup_active.store(false, Ordering::Release);
        shared.publish_session_work();
    }
    Ok(disposition)
}

fn handle_addr_resolved(
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    route: &Arc<OutboundRoute>,
) -> Result<EventDisposition> {
    let Some(OutboundState::AwaitAddr {
        cm_id,
        request,
        reservation,
    }) = route.take_state_if(|route_state| matches!(route_state, OutboundState::AwaitAddr { .. }))
    else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    if request.observer.cancelled.load(Ordering::Acquire) || shared.shutdown_requested() {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(route, true);
        request.complete(Err(Error::DriverShutdown));
        return Ok(EventDisposition::Handled);
    }
    if let Err(error) = cm_id.require_context(resources.context.raw_context()) {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(route, true);
        request.complete_failure(Error::from_v1(error));
        return Ok(EventDisposition::Handled);
    }
    match cm_id.resolve_route(2_000) {
        Ok(()) => route.set_state(OutboundState::AwaitRoute {
            cm_id,
            request,
            reservation,
        }),
        Err(error) => {
            state.defer_cm_id(cm_id);
            drop(reservation);
            state.retire_route(route, true);
            request.complete_failure(Error::from_v1(error));
        }
    }
    Ok(EventDisposition::Handled)
}

fn handle_route_resolved(
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    route: &Arc<OutboundRoute>,
) -> Result<EventDisposition> {
    let Some(OutboundState::AwaitRoute {
        cm_id,
        request,
        reservation,
    }) = route.take_state_if(|route_state| matches!(route_state, OutboundState::AwaitRoute { .. }))
    else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    if request.observer.cancelled.load(Ordering::Acquire) || shared.shutdown_requested() {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(route, true);
        request.complete(Err(Error::DriverShutdown));
        return Ok(EventDisposition::Handled);
    }
    if let Err(error) = cm_id.require_context(resources.context.raw_context()) {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(route, true);
        request.complete_failure(Error::from_v1(error));
        return Ok(EventDisposition::Handled);
    }

    let local_addr = cm_id.local_addr();
    let peer_addr = cm_id.peer_addr();
    let qp = match build_qp(resources, &cm_id, &request.config) {
        Ok(qp) => qp,
        Err(error) => {
            state.defer_cm_id(cm_id);
            drop(reservation);
            state.retire_route(route, true);
            request.complete_failure(error);
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
        Err(failure) => {
            let (error, failed_resources) = failure.into_parts();
            match shared.destroy_unregistered_connection(&verbs) {
                Ok((cm_id, _qp_destroyed)) => {
                    state.release_failed_install(shared, failed_resources)?;
                    if let Some(cm_id) = cm_id {
                        state.defer_cm_id(cm_id);
                    }
                    state.retire_route(route, true);
                }
                Err(destroy_error) => {
                    CmState::record_setup_rollback_quarantine(&destroy_error);
                    let connection =
                        state.retain_failed_install(shared, failed_resources, &destroy_error);
                    route.set_state(OutboundState::Quarantined { connection });
                }
            }
            drop(verbs);
            request.complete_failure(error);
            return Ok(EventDisposition::Handled);
        }
    };

    let Some(setup) = request.take_setup() else {
        drop(verbs);
        fail_registered_connection(
            state,
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
            fail_registered_connection(state, shared, route, request, connection, error)?;
            return Ok(EventDisposition::Handled);
        }
    };
    let establish = run_setup_before_establish(
        setup,
        &connection,
        || {
            if request.observer.cancelled.load(Ordering::Acquire) || shared.shutdown_requested() {
                Err(Error::DriverShutdown)
            } else {
                Ok(())
            }
        },
        || verbs.connect(&conn_param),
    );
    if let Err(error) = establish {
        drop(verbs);
        fail_registered_connection(state, shared, route, request, connection, error)?;
        return Ok(EventDisposition::Handled);
    }
    if request.observer.cancelled.load(Ordering::Acquire) || shared.shutdown_requested() {
        drop(verbs);
        fail_registered_connection(
            state,
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
    _state: &CmState,
    shared: &SessionManager,
    route: &Arc<OutboundRoute>,
    request: Arc<OutboundRequest>,
    connection: RdmaConnection,
    error: Error,
) -> Result<()> {
    let connection_state = connection.require_session_state()?;
    route.set_state(OutboundState::Closing {
        connection: EstablishedConnectionRoute::new(&connection_state),
    });
    shared.begin_connection_close(&connection_state);
    drop(connection);
    if connection_state.accepted_count() == 0 {
        shared.retire_registered_connection(connection_state.token)?;
    }
    if matches!(&error, Error::DriverShutdown) {
        request.complete(Err(error));
    } else {
        request.complete_failure(error);
    }
    Ok(())
}

fn handle_established(
    state: &CmState,
    shared: &SessionManager,
    route: &Arc<OutboundRoute>,
) -> Result<EventDisposition> {
    let Some(OutboundState::AwaitEstablished {
        request,
        connection,
    }) = route
        .take_state_if(|route_state| matches!(route_state, OutboundState::AwaitEstablished { .. }))
    else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    if request.observer.cancelled.load(Ordering::Acquire) || shared.shutdown_requested() {
        fail_registered_connection(
            state,
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
        connection: EstablishedConnectionRoute::new(&connection.require_session_state()?),
    });
    waiter.complete(Ok(connection));
    Ok(EventDisposition::Handled)
}

fn handle_disconnected(
    state: &CmState,
    shared: &SessionManager,
    route: &Arc<OutboundRoute>,
) -> Result<EventDisposition> {
    let route_state = route.take_state_if(|route_state| {
        matches!(
            route_state,
            OutboundState::EstablishedAwaitingDelivery { .. } | OutboundState::Established { .. }
        )
    });
    let Some(route_state) = route_state else {
        if route.is_disconnected() {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
    };
    let (request, connection) = match route_state {
        OutboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => (Some(request), connection),
        OutboundState::Established { connection } => (None, connection),
        _ => unreachable!("disconnect state was pre-filtered"),
    };
    let Some(connection_state) = connection.upgrade() else {
        state.retire_route(route, true);
        return Ok(EventDisposition::Handled);
    };
    if let Some(event) = connection_state.mark_disconnected() {
        event.deliver();
    }
    let awaiting_delivery = request
        .as_ref()
        .is_some_and(|request| !request.observer.delivered.load(Ordering::Acquire));
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
        shared.retire_registered_connection(connection_state.token)?;
    }
    Ok(EventDisposition::Handled)
}

fn handle_failure_event(
    state: &CmState,
    shared: &SessionManager,
    route: &Arc<OutboundRoute>,
    snapshot: CmEventSnapshot,
) -> Result<EventDisposition> {
    let message = format!(
        "RDMA CM {:?} failed with status {} for id={:#x} listen_id={:#x}",
        snapshot.event_type, snapshot.status, snapshot.id, snapshot.listen_id
    );
    let route_state = route.take_state_if(|_| true);
    let Some(route_state) = route_state else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    match route_state {
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
            let shutdown_won =
                request.observer.cancelled.load(Ordering::Acquire) || shared.shutdown_requested();
            state.defer_cm_id(cm_id);
            drop(reservation);
            state.retire_route(route, true);
            if shutdown_won {
                tracing::debug!(
                    cm_event = ?snapshot.event_type,
                    status = snapshot.status,
                    id = snapshot.id,
                    listen_id = snapshot.listen_id,
                    failure = %message,
                    "ignoring outbound CM setup failure after shutdown won the request"
                );
                request.complete(Err(Error::DriverShutdown));
                return Ok(EventDisposition::IgnoredAfterShutdown);
            }
            request.complete_failure(Error::Verbs(std::io::Error::other(message)));
        }
        OutboundState::AwaitEstablished {
            request,
            connection,
        } => {
            fail_registered_connection(
                state,
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
            let Some(connection_state) = connection.upgrade() else {
                state.retire_route(route, true);
                return Ok(EventDisposition::Handled);
            };
            if let Some(event) = connection_state
                .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())))
            {
                event.deliver();
            }
            if request.observer.delivered.load(Ordering::Acquire) {
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
                shared.retire_registered_connection(connection_state.token)?;
            }
        }
        OutboundState::Established { connection } | OutboundState::Disconnected { connection } => {
            let Some(connection_state) = connection.upgrade() else {
                state.retire_route(route, true);
                return Ok(EventDisposition::Handled);
            };
            if let Some(event) = connection_state
                .mark_cm_failure(Error::Verbs(std::io::Error::other(message.clone())))
            {
                event.deliver();
            }
            route.set_state(OutboundState::Failed {
                connection: connection.clone(),
            });
            shared.begin_connection_close(&connection_state);
            if connection_state.accepted_count() == 0 {
                shared.retire_registered_connection(connection_state.token)?;
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
        route_state @ OutboundState::Quarantined { .. } => {
            route.set_state(route_state);
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

pub(super) struct OutboundRequest {
    pub(super) address: SocketAddr,
    pub(super) config: RdmaConnectionConfig,
    setup: Mutex<Option<ConnectionSetup>>,
    reservation: Mutex<Option<ConnectionReservation>>,
    pub(super) observer: Arc<OutboundRequestObserver>,
    cancellation_enqueued: AtomicBool,
    pub(super) route_token: AtomicU64,
}

pub(super) struct OutboundRequestObserver {
    result: Mutex<TakeOnceResult<RdmaConnection>>,
    pub(super) cancelled: AtomicBool,
    pub(super) delivered: AtomicBool,
    waker: AtomicWaker,
}

impl OutboundRequest {
    pub(super) fn new(
        address: SocketAddr,
        config: RdmaConnectionConfig,
        setup: ConnectionSetup,
        reservation: ConnectionReservation,
    ) -> Self {
        Self {
            address,
            config,
            setup: Mutex::new(Some(setup)),
            reservation: Mutex::new(Some(reservation)),
            observer: Arc::new(OutboundRequestObserver {
                result: Mutex::new(TakeOnceResult::Pending),
                cancelled: AtomicBool::new(false),
                delivered: AtomicBool::new(false),
                waker: AtomicWaker::new(),
            }),
            cancellation_enqueued: AtomicBool::new(false),
            route_token: AtomicU64::new(0),
        }
    }

    pub(super) fn take_setup(&self) -> Option<ConnectionSetup> {
        lock_unpoison(&self.setup).take()
    }

    pub(super) fn take_reservation(&self) -> Option<ConnectionReservation> {
        lock_unpoison(&self.reservation).take()
    }

    pub(super) fn complete(&self, result: Result<RdmaConnection>) {
        let mut current = lock_unpoison(&self.observer.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(result);
            drop(current);
            self.observer.waker.wake();
        }
    }

    pub(super) fn complete_failure(&self, error: Error) {
        let mut current = lock_unpoison(&self.observer.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(Err(error));
            drop(current);
            self.observer.waker.wake();
        }
    }

    pub(super) fn try_enqueue_cancellation(&self) -> bool {
        !self.cancellation_enqueued.swap(true, Ordering::AcqRel)
    }

    pub(super) fn cancel(&self, error: Error) {
        self.observer.cancel(error);
    }

    pub(super) fn take_result(&self) -> Option<Result<RdmaConnection>> {
        self.observer.take_result()
    }
}

impl OutboundRequestObserver {
    pub(super) fn take_result(&self) -> Option<Result<RdmaConnection>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Ready(result) => Some(result),
            TakeOnceResult::Pending => {
                *current = TakeOnceResult::Pending;
                None
            }
            TakeOnceResult::Taken => None,
        }
    }

    fn cancel(&self, error: Error) {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let (replacement, undelivered) =
            match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
                TakeOnceResult::Pending => (TakeOnceResult::Ready(Err(error)), None),
                TakeOnceResult::Ready(Ok(connection)) => {
                    (TakeOnceResult::Ready(Err(error)), Some(connection))
                }
                TakeOnceResult::Ready(Err(existing)) => {
                    (TakeOnceResult::Ready(Err(existing)), None)
                }
                TakeOnceResult::Taken => (TakeOnceResult::Taken, None),
            };
        *current = replacement;
        drop(current);
        drop(undelivered);
        self.waker.wake();
    }
}

pub(super) struct ConnectWaiter {
    pub(super) manager: Weak<SessionManager>,
    pub(super) request: Weak<OutboundRequest>,
    pub(super) observer: Arc<OutboundRequestObserver>,
    pub(super) finished: bool,
}

impl Future for ConnectWaiter {
    type Output = Result<RdmaConnection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.observer.take_result() {
            if result.is_ok() {
                self.mark_delivered();
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        self.observer.waker.register(cx.waker());
        if let Some(result) = self.observer.take_result() {
            if result.is_ok() {
                self.mark_delivered();
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        Poll::Pending
    }
}

impl ConnectWaiter {
    fn mark_delivered(&self) {
        self.observer.delivered.store(true, Ordering::Release);
        let Some(request) = self.request.upgrade() else {
            return;
        };
        if let Some(manager) = self.manager.upgrade() {
            manager.cm.mark_request_delivered(&request);
        }
    }
}

impl Drop for ConnectWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.observer.cancel(Error::DriverShutdown);
        if let Some(manager) = self.manager.upgrade() {
            let Some(request) = self.request.upgrade() else {
                return;
            };
            manager.cm.enqueue_cancellation(request);
            manager.publish_session_work();
        }
    }
}
