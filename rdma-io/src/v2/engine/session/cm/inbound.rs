//! Listener creation, inbound admission, acceptance, and CM transitions.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rdma_io_sys::rdmacm::rdma_cm_id;

use super::{
    AcceptRequest, CmEventReject, CmEventSnapshot, CmRouteToken, CmState, ConnectionCmRoute,
    ContextRoute, EngineResources, EstablishedConnectionRoute, EventDisposition,
    InboundRejectReason, InboundRoute, InboundState, IncomingChild, KERNEL_LISTEN_BACKLOG_REQUEST,
    ListenRequest, ListenerAction, ListenerState, Lookup, PendingCmDestruction, RdmaConnection,
    RdmaListener, SessionManager, SharedCmId, VerbsConnectionResources, build_qp,
    contextual_cm_error, install_reserved_connection, is_failure_event, lock_unpoison,
    reserve_connection, run_setup_before_establish,
};
use crate::cm::{CmEventType, CmId, PortSpace};
use crate::v2::error::{Error, Result};

pub(super) fn start_listener(
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    request: Arc<ListenRequest>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    if request.is_cancelled()
        || state.shutting_down.load(Ordering::Acquire)
        || shared.shutdown_requested()
    {
        request.complete_into(Err(Error::DriverShutdown), actions);
        return Ok(());
    }
    let token = state
        .next_listener_token
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| Error::CapacityExhausted)?;
    let cm_id =
        match CmId::new_with_context_token(&resources.cm_event_channel, PortSpace::Tcp, token) {
            Ok(cm_id) => SharedCmId::new(cm_id, Arc::clone(&resources.cm_event_channel)),
            Err(error) => {
                request.complete_into(
                    Err(contextual_cm_error(
                        format!("create listener {}", request.address),
                        Error::from_v1(error),
                    )),
                    actions,
                );
                return Ok(());
            }
        };
    let context_key = cm_id.context_key();
    let raw_id = cm_id.as_raw() as usize;
    if !state.insert_context_route(context_key, ContextRoute::Listener { token, raw_id }) {
        state.defer_cm_id(cm_id);
        request.complete_into(
            Err(Error::InvalidConfig(
                "duplicate listener CM context identity".into(),
            )),
            actions,
        );
        return Ok(());
    }
    if let Err(error) = cm_id.listen(&request.address, KERNEL_LISTEN_BACKLOG_REQUEST) {
        state.defer_cm_id(cm_id);
        request.complete_into(
            Err(contextual_cm_error(
                format!(
                    "listen on {} with requested kernel backlog {}",
                    request.address, KERNEL_LISTEN_BACKLOG_REQUEST
                ),
                Error::from_v1(error),
            )),
            actions,
        );
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
            state.defer_cm_id(cm_id);
            request.complete_into(Err(error), actions);
            return Ok(());
        }
    };
    let listener = Arc::new(ListenerState::new(
        token,
        local_addr,
        request.config.clone(),
        cm_id,
    ));
    if !state.insert_listener_identity(token, raw_id, Arc::clone(&listener)) {
        let cm_id = listener
            .take_cm_id()
            .expect("new duplicate listener still owns its CM ID");
        state.defer_cm_id(cm_id);
        request.complete_into(
            Err(Error::InvalidConfig(
                "duplicate listener route identity".into(),
            )),
            actions,
        );
        return Ok(());
    }
    if request.is_cancelled() {
        listener.request_close(shared);
        request.complete_into(Err(Error::DriverShutdown), actions);
    } else {
        request.complete_into(Ok(RdmaListener::from_state(shared, listener)), actions);
    }
    Ok(())
}

pub(super) fn service_listener(
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    listener: &Arc<ListenerState>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    match listener.next_action() {
        ListenerAction::CancelledBeforeSelection(request) => {
            request.complete_into(Err(Error::DriverShutdown), actions);
        }
        ListenerAction::FailUnselected(request) => {
            request.complete_into(Err(listener.close_error()), actions);
        }
        ListenerAction::RejectChild(child, reason) => {
            reject_child(state, child, reason)?;
        }
        ListenerAction::ProcessSelected { request, child } => {
            process_selected_pair(state, shared, resources, listener, request, child, actions)?;
        }
        ListenerAction::RejectSelected {
            request,
            child,
            reason,
        } => {
            reject_child(state, child, InboundRejectReason::ListenerClosed)?;
            request.complete_into(Err(reason), actions);
            listener.finish_selected_request(&request);
        }
        ListenerAction::CancelAfterAccept { request, route } => {
            let _ = request;
            cancel_inbound_route(state, shared, route, actions)?;
        }
        ListenerAction::FinalizeClose => {
            finalize_listener(state, listener)?;
        }
        ListenerAction::None => {}
    }
    Ok(())
}

pub(super) fn handle_connect_request(
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    listener: &Arc<ListenerState>,
    snapshot: CmEventSnapshot,
) -> Result<EventDisposition> {
    if snapshot.status != 0 || listener.is_closing() {
        reject_raw_child(
            state,
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
    if let Err(error) = child_id.require_context(resources.context.raw_context()) {
        tracing::warn!(
            listener = %listener.local_addr,
            "rejecting inbound child with mismatched verbs context: {error}"
        );
        reject_unreserved_child(state, child_id, InboundRejectReason::ContextMismatch)?;
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
            reject_unreserved_child(state, child_id, reason)?;
            return Ok(EventDisposition::Handled);
        }
    };
    let admitted = listener.admit_child(IncomingChild::new(child_id, reservation));
    drop(admission);
    if let Some((child, reason)) = admitted.rejected {
        reject_child(state, child, reason)?;
    }
    if listener.has_work() {
        state.enqueue_listener_work(listener);
    }
    Ok(EventDisposition::Handled)
}

pub(super) fn handle_listener_event(
    _state: &CmState,
    shared: &SessionManager,
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
    state: &CmState,
    shared: &SessionManager,
    resources: &EngineResources,
    listener: &Arc<ListenerState>,
    request: Arc<AcceptRequest>,
    child: IncomingChild,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    if request.is_cancelled() || listener.is_closing() || shared.shutdown_requested() {
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
        reject_child(state, child, reject)?;
        request.complete_into(Err(error), actions);
        listener.finish_selected_request(&request);
        return Ok(());
    }
    let intent = request.take_intent().ok_or_else(|| {
        Error::InvalidConfig("selected accept intent was consumed more than once".into())
    })?;
    let (config, setup) = intent.into_parts()?;
    let (mut child_cm_id, child_reservation) = child.into_resources()?;
    let (token, route) = state
        .inbound_routes
        .allocate_with(|token| Arc::new(InboundRoute::new(token, Arc::downgrade(listener))))?;
    request.set_route_token(token.encode());
    if let Err(error) = child_cm_id.install_context_token(token.encode()) {
        state.inbound_routes.release(token, false);
        reject_unreserved_child(state, child_cm_id, InboundRejectReason::SetupFailure)?;
        drop(child_reservation);
        request.complete_into(Err(error), actions);
        listener.finish_selected_request(&request);
        return Ok(());
    }
    let raw_id = child_cm_id.as_raw() as usize;
    let context_key = child_cm_id.context_key();
    route.set_identity(raw_id, context_key);
    if !state.insert_context_route(context_key, ContextRoute::Inbound { token, raw_id }) {
        state.inbound_routes.release(token, false);
        reject_unreserved_child(state, child_cm_id, InboundRejectReason::SetupFailure)?;
        drop(child_reservation);
        request.complete_into(
            Err(Error::InvalidConfig(
                "duplicate inbound CM context identity".into(),
            )),
            actions,
        );
        listener.finish_selected_request(&request);
        return Ok(());
    }
    listener.route_selected(&request, token.encode())?;

    let local_addr = child_cm_id.local_addr();
    let peer_addr = child_cm_id.peer_addr();
    let qp = match build_qp(resources, &child_cm_id, &config) {
        Ok(qp) => qp,
        Err(error) => {
            state.remove_owned_context_route(Some(&child_cm_id));
            state.inbound_routes.release(token, false);
            reject_unreserved_child(state, child_cm_id, InboundRejectReason::SetupFailure)?;
            drop(child_reservation);
            request.complete_into(
                Err(contextual_cm_error(
                    format!("build inbound QP for {}", listener.local_addr),
                    error,
                )),
                actions,
            );
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
        Err(failure) => {
            let (error, failed_resources) = failure.into_parts();
            match shared.destroy_unregistered_connection(&verbs) {
                Ok((cm_id, _qp_destroyed)) => {
                    state.release_failed_install(shared, failed_resources)?;
                    if let Some(cm_id) = cm_id {
                        reject_unreserved_child(state, cm_id, InboundRejectReason::SetupFailure)?;
                    }
                    state.inbound_routes.release(token, false);
                }
                Err(destroy_error) => {
                    CmState::record_setup_rollback_quarantine(&destroy_error);
                    reject_retained_inbound_child(&verbs);
                    let connection = state.retain_failed_install(
                        shared,
                        failed_resources,
                        &destroy_error,
                        actions,
                    );
                    route.set_state(InboundState::Quarantined { connection });
                }
            }
            request.complete_into(Err(error), actions);
            listener.finish_selected_route(token.encode());
            return Ok(());
        }
    };

    let conn_param = match config.conn_param() {
        Ok(param) => param,
        Err(error) => {
            drop(verbs);
            fail_selected_connection(state, shared, &route, request, connection, error, actions)?;
            return Ok(());
        }
    };
    let establish = run_setup_before_establish(
        setup,
        &connection,
        || {
            if request.is_cancelled() || listener.is_closing() || shared.shutdown_requested() {
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
        fail_selected_connection(state, shared, &route, request, connection, error, actions)?;
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
    _state: &CmState,
    shared: &SessionManager,
    route: &Arc<InboundRoute>,
    request: Arc<AcceptRequest>,
    connection: RdmaConnection,
    error: Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let connection_state = connection.require_session_state()?;
    let reject = match &error {
        Error::DriverShutdown if !request.is_cancelled() => InboundRejectReason::AdmissionClosed,
        Error::DriverShutdown | Error::TransportClosed => InboundRejectReason::ListenerClosed,
        _ => InboundRejectReason::SetupFailure,
    };
    route.set_state(InboundState::Closing {
        connection: EstablishedConnectionRoute::new(&connection_state),
        request: Some(request),
        completion: Some(error),
        selected: true,
        reject: Some(reject),
    });
    shared.begin_connection_close_into(&connection_state, actions);
    drop(connection);
    if connection_state.accepted_count() == 0 {
        shared.retire_registered_connection_into(connection_state.token, actions)?;
    }
    Ok(())
}

pub(super) fn reject_raw_child(
    state: &CmState,
    resources: &EngineResources,
    raw_id: usize,
    reason: InboundRejectReason,
) -> Result<()> {
    if raw_id == 0 {
        return Ok(());
    }
    let cm_id = unsafe { CmId::from_raw(raw_id as *mut rdma_cm_id, true) };
    reject_unreserved_child(
        state,
        SharedCmId::new(cm_id, Arc::clone(&resources.cm_event_channel)),
        reason,
    )
}

fn reject_unreserved_child(
    state: &CmState,
    cm_id: SharedCmId,
    reason: InboundRejectReason,
) -> Result<()> {
    cm_id.reject(&[]).map_err(|error| {
        contextual_cm_error(
            format!("reject inbound child ({reason:?})"),
            Error::from_v1(error),
        )
    })?;
    state.defer_cm_id(cm_id);
    Ok(())
}

fn reject_retained_inbound_child(verbs: &VerbsConnectionResources) {
    if let Err(error) = verbs.reject() {
        tracing::warn!(
            %error,
            "failed to reject inbound child before retaining setup rollback quarantine"
        );
    }
}

fn reject_child(state: &CmState, child: IncomingChild, reason: InboundRejectReason) -> Result<()> {
    let (cm_id, reservation) = child.into_resources()?;
    let result = reject_unreserved_child(state, cm_id, reason);
    drop(reservation);
    result
}

fn cancel_inbound_route(
    state: &CmState,
    shared: &SessionManager,
    encoded: u64,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let token = CmRouteToken::decode(encoded);
    let Lookup::Occupied(route) = state.inbound_routes.lookup_cloned(token) else {
        return Ok(());
    };
    let route_state = route.take_state_if(|route_state| {
        matches!(
            route_state,
            InboundState::AwaitEstablished { .. }
                | InboundState::EstablishedAwaitingDelivery { .. }
        )
    });
    let Some(route_state) = route_state else {
        return Ok(());
    };
    let cancellation_error = || {
        route
            .listener
            .upgrade()
            .filter(|listener| listener.is_closing())
            .map_or(Error::DriverShutdown, |listener| listener.close_error())
    };
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let connection_state = connection.require_session_state()?;
            let error = cancellation_error();
            route.set_state(InboundState::Closing {
                connection: EstablishedConnectionRoute::new(&connection_state),
                request: Some(request),
                completion: Some(error),
                selected: true,
                reject: None,
            });
            shared.begin_connection_close_into(&connection_state, actions);
            drop(connection);
            if connection_state.accepted_count() == 0 {
                shared.retire_registered_connection_into(connection_state.token, actions)?;
            }
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => {
            let Some(connection_state) = connection.upgrade() else {
                state.inbound_routes.release(token, true);
                return Ok(());
            };
            let error = cancellation_error();
            if request.fail_undelivered_into(error, actions) {
                route.set_state(InboundState::Established {
                    connection: connection.clone(),
                });
                if let Some(listener) = route.listener.upgrade()
                    && listener.finish_selected_route(encoded)
                {
                    state.enqueue_listener_work(&listener);
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
            shared.begin_connection_close_into(&connection_state, actions);
            if connection_state.accepted_count() == 0 {
                shared.retire_registered_connection_into(connection_state.token, actions)?;
            }
        }
        _ => unreachable!("inbound cancellation state was pre-filtered"),
    }
    Ok(())
}

fn finalize_listener(state: &CmState, listener: &Arc<ListenerState>) -> Result<()> {
    let Some(cm_id) = listener.take_cm_id() else {
        return Ok(());
    };
    let raw_id = cm_id.as_raw() as usize;
    let mut listeners = lock_unpoison(&state.listeners);
    let mut listener_ids = lock_unpoison(&state.listener_ids);
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
    lock_unpoison(&state.cm_destructions).push_back(PendingCmDestruction::Listener {
        cm_id,
        listener: Arc::clone(listener),
    });
    Ok(())
}

pub(super) fn handle_event(
    state: &CmState,
    shared: &SessionManager,
    route: &Arc<InboundRoute>,
    snapshot: CmEventSnapshot,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    if is_failure_event(snapshot.event_type) || snapshot.status != 0 {
        return handle_failure(state, shared, route, snapshot, actions);
    }
    match snapshot.event_type {
        CmEventType::Established => {
            let Some(InboundState::AwaitEstablished {
                request,
                connection,
            }) = route.take_state_if(|route_state| {
                matches!(route_state, InboundState::AwaitEstablished { .. })
            })
            else {
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            };
            let listener = route.listener.upgrade();
            if request.is_cancelled()
                || listener
                    .as_ref()
                    .is_none_or(|listener| listener.is_closing())
                || shared.shutdown_requested()
            {
                let connection_state = connection.require_session_state()?;
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
                shared.begin_connection_close_into(&connection_state, actions);
                drop(connection);
                if connection_state.accepted_count() == 0 {
                    shared.retire_registered_connection_into(connection_state.token, actions)?;
                }
                return Ok(EventDisposition::Handled);
            }

            let connection_state = connection.require_session_state()?;
            let connection_route = EstablishedConnectionRoute::new(&connection_state);
            route.set_state(InboundState::EstablishedAwaitingDelivery {
                request: Arc::clone(&request),
                connection: connection_route,
            });
            request.complete_success_into(connection, actions);
            Ok(EventDisposition::Handled)
        }
        CmEventType::Disconnected => handle_disconnected(state, shared, route, actions),
        CmEventType::TimewaitExit => Ok(EventDisposition::Handled),
        _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
    }
}

pub(super) fn handle_disconnected(
    state: &CmState,
    shared: &SessionManager,
    route: &Arc<InboundRoute>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let route_state = route.take_state_if(|route_state| {
        matches!(
            route_state,
            InboundState::AwaitEstablished { .. }
                | InboundState::EstablishedAwaitingDelivery { .. }
                | InboundState::Established { .. }
        )
    });
    let Some(route_state) = route_state else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    let (connection, request, selected) = match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => (
            EstablishedConnectionRoute::new(&connection.require_session_state()?),
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
            let _ = request.fail_undelivered_into(
                Error::Verbs(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "inbound disconnect lost connection state before accept retirement",
                )),
                actions,
            );
        }
        if selected
            && let Some(listener) = route.listener.upgrade()
            && listener.finish_selected_route(route.token.encode())
        {
            state.enqueue_listener_work(&listener);
        }
        state.inbound_routes.release(route.token, true);
        return Ok(EventDisposition::Handled);
    };
    if let Some(event) = connection_state.mark_disconnected() {
        actions.push_event(event);
    }
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
        && request.fail_undelivered_into(
            Error::Verbs(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "inbound connection disconnected before accept delivery",
            )),
            actions,
        )
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
            state.enqueue_listener_work(&listener);
        }
    }
    shared.begin_connection_close_into(&connection_state, actions);
    if connection_state.accepted_count() == 0 {
        shared.retire_registered_connection_into(connection_state.token, actions)?;
    }
    Ok(EventDisposition::Handled)
}

fn handle_failure(
    state: &CmState,
    shared: &SessionManager,
    route: &Arc<InboundRoute>,
    snapshot: CmEventSnapshot,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let message = format!(
        "inbound RDMA CM {:?} failed with status {} for id={:#x} listen_id={:#x}",
        snapshot.event_type, snapshot.status, snapshot.id, snapshot.listen_id
    );
    let route_state = route.take_state_if(|_| true);
    let Some(route_state) = route_state else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let connection_state = connection.require_session_state()?;
            if let Some(event) = connection_state.mark_cm_failure_into(
                Error::Verbs(std::io::Error::other(message.clone())),
                actions,
            ) {
                actions.push_event(event);
            }
            route.set_state(InboundState::Closing {
                connection: EstablishedConnectionRoute::new(&connection_state),
                request: Some(request),
                completion: Some(Error::Verbs(std::io::Error::other(message))),
                selected: true,
                reject: None,
            });
            shared.begin_connection_close_into(&connection_state, actions);
            drop(connection);
            if connection_state.accepted_count() == 0 {
                shared.retire_registered_connection_into(connection_state.token, actions)?;
            }
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => {
            let Some(connection_state) = connection.upgrade() else {
                state.inbound_routes.release(route.token, true);
                return Ok(EventDisposition::Handled);
            };
            if let Some(event) = connection_state.mark_cm_failure_into(
                Error::Verbs(std::io::Error::other(message.clone())),
                actions,
            ) {
                actions.push_event(event);
            }
            route.set_state(InboundState::Closing {
                connection: connection.clone(),
                request: Some(Arc::clone(&request)),
                completion: Some(Error::Verbs(std::io::Error::other(message.clone()))),
                selected: true,
                reject: None,
            });
            if request.fail_undelivered_into(Error::Verbs(std::io::Error::other(message)), actions)
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
                    state.enqueue_listener_work(&listener);
                }
            }
            shared.begin_connection_close_into(&connection_state, actions);
            if connection_state.accepted_count() == 0 {
                shared.retire_registered_connection_into(connection_state.token, actions)?;
            }
        }
        InboundState::Established { connection } => {
            let Some(connection_state) = connection.upgrade() else {
                state.inbound_routes.release(route.token, true);
                return Ok(EventDisposition::Handled);
            };
            if let Some(event) = connection_state.mark_cm_failure_into(
                Error::Verbs(std::io::Error::other(message.clone())),
                actions,
            ) {
                actions.push_event(event);
            }
            route.set_state(InboundState::Closing {
                connection: connection.clone(),
                request: None,
                completion: None,
                selected: false,
                reject: None,
            });
            shared.begin_connection_close_into(&connection_state, actions);
            if connection_state.accepted_count() == 0 {
                shared.retire_registered_connection_into(connection_state.token, actions)?;
            }
        }
        route_state @ InboundState::Closing { .. } => {
            route.set_state(route_state);
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        route_state @ InboundState::Quarantined { .. } => {
            route.set_state(route_state);
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
