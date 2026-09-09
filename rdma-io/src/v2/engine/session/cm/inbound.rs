//! Listener creation, inbound admission, acceptance, and CM transitions.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rdma_io_sys::rdmacm::rdma_cm_id;

use super::super::connection::ConnectionState;
use super::{
    AcceptRequest, CmEventReject, CmEventSnapshot, CmRouteToken, CmState, ContextRoute,
    EngineResources, EstablishedConnectionRoute, EventDisposition, InboundRejectReason,
    InboundRoute, InboundState, IncomingChild, KERNEL_LISTEN_BACKLOG_REQUEST, ListenRequest,
    ListenerAction, ListenerState, Lookup, PendingCmDestruction, RdmaConnection, RdmaListener,
    SessionManager, SharedCmId, VerbsConnectionResources, build_qp, contextual_cm_error,
    install_reserved_connection, is_failure_event, lock_unpoison, reserve_connection,
    run_setup_before_establish,
};
use crate::cm::{CmEventType, CmId, PortSpace};
use crate::v2::engine::session::registry::{ConnectionRegistry, ConnectionSnapshot};
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
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
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
            reject_child(state, connections, child, reason)?;
        }
        ListenerAction::ProcessSelected { request, child } => {
            process_selected_pair(
                state,
                connections,
                shared,
                io_core,
                resources,
                listener,
                request,
                child,
                actions,
            )?;
        }
        ListenerAction::RejectSelected {
            request,
            child,
            reason,
        } => {
            reject_child(
                state,
                connections,
                child,
                InboundRejectReason::ListenerClosed,
            )?;
            request.complete_into(Err(reason), actions);
            listener.finish_selected_request(&request);
        }
        ListenerAction::CancelAfterAccept { request, route } => {
            let _ = request;
            cancel_inbound_route(state, connections, shared, io_core, route, actions)?;
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
    connections: &mut ConnectionRegistry,
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

    let (admission, reservation) = match reserve_connection(shared, connections) {
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
    let token = match connections.register_inbound(|token| {
        let mut route = InboundRoute::new(token, Arc::downgrade(listener));
        route.set_state(InboundState::PendingSelection {
            cm_id: child_id,
            reservation,
        });
        route
    }) {
        Ok(token) => token,
        Err(error) => {
            return Err(error);
        }
    };
    let admitted = listener.admit_child(IncomingChild::new(token));
    drop(admission);
    if let Some((child, reason)) = admitted.rejected {
        reject_child(state, connections, child, reason)?;
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

#[allow(
    clippy::too_many_arguments,
    reason = "CM transition dependencies stay explicit"
)]
fn process_selected_pair(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
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
        reject_child(state, connections, child, reject)?;
        request.complete_into(Err(error), actions);
        listener.finish_selected_request(&request);
        return Ok(());
    }
    let intent = request.take_intent().ok_or_else(|| {
        Error::InvalidConfig("selected accept intent was consumed more than once".into())
    })?;
    let (config, setup) = intent.into_parts()?;
    let token = child.into_token();
    let Some(InboundState::PendingSelection {
        cm_id: mut child_cm_id,
        reservation: child_reservation,
    }) = connections.take_inbound_state_if(token, |state| {
        matches!(state, InboundState::PendingSelection { .. })
    })
    else {
        return Err(Error::InvalidConfig(
            "selected inbound child lost its owning connection entry".into(),
        ));
    };
    request.set_route_token(token.encode());
    if let Err(error) = child_cm_id.install_context_token(token.encode()) {
        connections.release_route(token, false);
        reject_unreserved_child(state, child_cm_id, InboundRejectReason::SetupFailure)?;
        drop(child_reservation);
        request.complete_into(Err(error), actions);
        listener.finish_selected_request(&request);
        return Ok(());
    }
    let raw_id = child_cm_id.as_raw() as usize;
    let context_key = child_cm_id.context_key();
    assert!(connections.set_route_identity(token, raw_id, context_key));
    if !state.insert_context_route(context_key, ContextRoute::Inbound { token, raw_id }) {
        connections.release_route(token, false);
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
            connections.release_route(token, false);
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
    let verbs = VerbsConnectionResources::new_shared(qp, child_cm_id);
    let connection = match install_reserved_connection(
        shared,
        connections,
        Some(token),
        verbs,
        config.clone(),
        local_addr,
        peer_addr,
        child_reservation,
    ) {
        Ok(connection) => connection,
        Err(failure) => {
            let (error, mut failed_resources) = failure.into_parts();
            match shared.destroy_failed_connection_install(connections, &mut failed_resources) {
                Ok((cm_id, _qp_destroyed)) => {
                    state.release_failed_install(connections, failed_resources)?;
                    if let Some(cm_id) = cm_id {
                        reject_unreserved_child(state, cm_id, InboundRejectReason::SetupFailure)?;
                    }
                    connections.release_route(token, false);
                }
                Err(destroy_error) => {
                    CmState::record_setup_rollback_quarantine(&destroy_error);
                    if let Err(error) =
                        shared.reject_failed_connection_install(connections, &failed_resources)
                    {
                        tracing::warn!(
                            %error,
                            "failed to reject inbound child before retaining setup rollback quarantine"
                        );
                    }
                    let connection = state.retain_failed_install(
                        connections,
                        shared,
                        token,
                        failed_resources,
                        &destroy_error,
                        actions,
                    );
                    connections.set_inbound_state(token, InboundState::Quarantined { connection });
                }
            }
            request.complete_into(Err(error), actions);
            listener.finish_selected_route(token.encode());
            return Ok(());
        }
    };
    registered_connection(connections, &connection)?;

    let conn_param = match config.conn_param() {
        Ok(param) => param,
        Err(error) => {
            fail_selected_connection(
                state,
                connections,
                shared,
                io_core,
                token,
                request,
                connection,
                error,
                actions,
            )?;
            return Ok(());
        }
    };
    let establish = run_setup_before_establish(
        setup,
        &connection,
        connections,
        connection.session_token(),
        io_core,
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
        |connections| {
            connections
                .with_connection(connection.session_token(), |connection| {
                    connection.accept(&conn_param)
                })
                .ok_or(Error::TransportClosed)?
        },
    );
    if let Err(error) = establish {
        fail_selected_connection(
            state,
            connections,
            shared,
            io_core,
            token,
            request,
            connection,
            error,
            actions,
        )?;
        return Ok(());
    }
    connections.set_inbound_state(
        token,
        InboundState::AwaitEstablished {
            request,
            connection,
        },
    );
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "CM failure transition dependencies stay explicit"
)]
fn fail_selected_connection(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    request: Arc<AcceptRequest>,
    connection: RdmaConnection,
    error: Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let connection_token = connection.session_token();
    registered_connection(connections, &connection)?;
    let reject = match &error {
        Error::DriverShutdown if !request.is_cancelled() => InboundRejectReason::AdmissionClosed,
        Error::DriverShutdown | Error::TransportClosed => InboundRejectReason::ListenerClosed,
        _ => InboundRejectReason::SetupFailure,
    };
    connections.set_inbound_state(
        token,
        InboundState::Closing {
            connection: EstablishedConnectionRoute::new(connection_token),
            request: Some(request),
            completion: Some(error),
            selected: true,
            reject: Some(reject),
        },
    );
    shared.begin_connection_close_into(connections, connection_token, io_core, actions);
    drop(connection);
    if connections
        .with_connection(connection_token, |connection| {
            connection.io_ledger.accepted_count()
        })
        .unwrap_or(0)
        == 0
    {
        super::retirement::retire_registered_connection_into(
            state,
            connections,
            shared,
            io_core,
            connection_token,
            actions,
        )?;
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

fn reject_child(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    child: IncomingChild,
    reason: InboundRejectReason,
) -> Result<()> {
    let token = child.into_token();
    let Some(InboundState::PendingSelection { cm_id, reservation }) = connections
        .take_inbound_state_if(token, |state| {
            matches!(state, InboundState::PendingSelection { .. })
        })
    else {
        return Ok(());
    };
    match cm_id.reject(&[]) {
        Ok(()) => {
            state.defer_cm_id(cm_id);
            drop(reservation);
            connections.release_route(token, true);
            Ok(())
        }
        Err(error) => {
            connections
                .set_inbound_state(token, InboundState::PendingSelection { cm_id, reservation });
            connections.track_bundle_quarantine(token);
            Err(contextual_cm_error(
                format!("reject inbound child ({reason:?})"),
                Error::from_v1(error),
            ))
        }
    }
}

fn cancel_inbound_route(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    encoded: u64,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let token = CmRouteToken::decode(encoded);
    let Lookup::Occupied(_) = connections.lookup_inbound(token) else {
        return Ok(());
    };
    let route_state = connections.take_inbound_state_if(token, |route_state| {
        matches!(
            route_state,
            InboundState::AwaitEstablished { .. }
                | InboundState::EstablishedAwaitingDelivery { .. }
        )
    });
    let Some(route_state) = route_state else {
        return Ok(());
    };
    let listener = connections.inbound_listener(token);
    let cancellation_error = || {
        listener
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .filter(|listener| listener.is_closing())
            .map_or(Error::DriverShutdown, |listener| listener.close_error())
    };
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let connection_token = connection.session_token();
            registered_connection(connections, &connection)?;
            let error = cancellation_error();
            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection: EstablishedConnectionRoute::new(connection_token),
                    request: Some(request),
                    completion: Some(error),
                    selected: true,
                    reject: None,
                },
            );
            shared.begin_connection_close_into(connections, connection_token, io_core, actions);
            drop(connection);
            if connections
                .with_connection(connection_token, |connection| {
                    connection.io_ledger.accepted_count()
                })
                .unwrap_or(0)
                == 0
            {
                super::retirement::retire_registered_connection_into(
                    state,
                    connections,
                    shared,
                    io_core,
                    connection_token,
                    actions,
                )?;
            }
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => {
            let connection_token = connection.token;
            if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
                connections.release_route(token, true);
                return Ok(());
            }
            let error = cancellation_error();
            if request.fail_undelivered_into(error, actions) {
                connections.set_inbound_state(
                    token,
                    InboundState::Established {
                        connection: connection.clone(),
                    },
                );
                if let Some(listener) = listener.as_ref().and_then(std::sync::Weak::upgrade)
                    && listener.finish_selected_route(encoded)
                {
                    state.enqueue_listener_work(&listener);
                }
                return Ok(());
            }
            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection: connection.clone(),
                    request: None,
                    completion: None,
                    selected: true,
                    reject: None,
                },
            );
            shared.begin_connection_close_into(connections, connection_token, io_core, actions);
            if connections
                .with_connection(connection_token, |connection| {
                    connection.io_ledger.accepted_count()
                })
                .unwrap_or(0)
                == 0
            {
                super::retirement::retire_registered_connection_into(
                    state,
                    connections,
                    shared,
                    io_core,
                    connection_token,
                    actions,
                )?;
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
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    snapshot: CmEventSnapshot,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    if is_failure_event(snapshot.event_type) || snapshot.status != 0 {
        return handle_failure(
            state,
            connections,
            shared,
            io_core,
            token,
            snapshot,
            actions,
        );
    }
    match snapshot.event_type {
        CmEventType::Established => {
            let Some(InboundState::AwaitEstablished {
                request,
                connection,
            }) = connections.take_inbound_state_if(token, |route_state| {
                matches!(route_state, InboundState::AwaitEstablished { .. })
            })
            else {
                return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
            };
            let listener = connections
                .inbound_listener(token)
                .and_then(|listener| listener.upgrade());
            if request.is_cancelled()
                || listener
                    .as_ref()
                    .is_none_or(|listener| listener.is_closing())
                || shared.shutdown_requested()
            {
                let connection_token = connection.session_token();
                registered_connection(connections, &connection)?;
                let completion = if request.is_cancelled() {
                    None
                } else if let Some(listener) = listener.as_ref() {
                    Some(listener.close_error())
                } else {
                    Some(Error::DriverShutdown)
                };
                connections.set_inbound_state(
                    token,
                    InboundState::Closing {
                        connection: EstablishedConnectionRoute::new(connection_token),
                        request: Some(request),
                        completion,
                        selected: true,
                        reject: None,
                    },
                );
                shared.begin_connection_close_into(connections, connection_token, io_core, actions);
                drop(connection);
                if connections
                    .with_connection(connection_token, |connection| {
                        connection.io_ledger.accepted_count()
                    })
                    .unwrap_or(0)
                    == 0
                {
                    super::retirement::retire_registered_connection_into(
                        state,
                        connections,
                        shared,
                        io_core,
                        connection_token,
                        actions,
                    )?;
                }
                return Ok(EventDisposition::Handled);
            }

            let connection_token = connection.session_token();
            registered_connection(connections, &connection)?;
            let connection_route = EstablishedConnectionRoute::new(connection_token);
            connections.set_inbound_state(
                token,
                InboundState::EstablishedAwaitingDelivery {
                    request: Arc::clone(&request),
                    connection: connection_route,
                },
            );
            connections.mark_active(connection_token);
            request.complete_success_into(connection, actions);
            Ok(EventDisposition::Handled)
        }
        CmEventType::Disconnected => {
            handle_disconnected(state, connections, shared, io_core, token, actions)
        }
        CmEventType::TimewaitExit => Ok(EventDisposition::Handled),
        _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
    }
}

pub(super) fn handle_disconnected(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let route_state = connections.take_inbound_state_if(token, |route_state| {
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
            EstablishedConnectionRoute::new(connection.session_token()),
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
    let connection_token = connection.token;
    if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
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
            && let Some(listener) = connections
                .inbound_listener(token)
                .and_then(|listener| listener.upgrade())
            && listener.finish_selected_route(token.encode())
        {
            state.enqueue_listener_work(&listener);
        }
        connections.release_route(token, true);
        return Ok(EventDisposition::Handled);
    }
    if let Some(event) = connections
        .with_connection_mut(connection_token, ConnectionState::mark_disconnected)
        .flatten()
    {
        actions.push_event(event);
    }
    connections.set_inbound_state(
        token,
        InboundState::Closing {
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
        },
    );
    if let Some(request) = request
        && request.fail_undelivered_into(
            Error::Verbs(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "inbound connection disconnected before accept delivery",
            )),
            actions,
        )
    {
        connections.set_inbound_state(
            token,
            InboundState::Closing {
                connection: connection.clone(),
                request: None,
                completion: None,
                selected: false,
                reject: None,
            },
        );
        if let Some(listener) = connections
            .inbound_listener(token)
            .and_then(|listener| listener.upgrade())
            && listener.finish_selected_route(token.encode())
        {
            state.enqueue_listener_work(&listener);
        }
    }
    shared.begin_connection_close_into(connections, connection_token, io_core, actions);
    if connections
        .with_connection(connection_token, |connection| {
            connection.io_ledger.accepted_count()
        })
        .unwrap_or(0)
        == 0
    {
        super::retirement::retire_registered_connection_into(
            state,
            connections,
            shared,
            io_core,
            connection_token,
            actions,
        )?;
    }
    Ok(EventDisposition::Handled)
}

fn registered_connection(
    connections: &ConnectionRegistry,
    connection: &RdmaConnection,
) -> Result<ConnectionSnapshot> {
    match connections.lookup(connection.session_token()) {
        Lookup::Occupied(connection) => Ok(connection),
        Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
            Err(Error::InvalidConfig(
                "registered inbound connection disappeared during CM transition".into(),
            ))
        }
    }
}

fn handle_failure(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    snapshot: CmEventSnapshot,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let message = format!(
        "inbound RDMA CM {:?} failed with status {} for id={:#x} listen_id={:#x}",
        snapshot.event_type, snapshot.status, snapshot.id, snapshot.listen_id
    );
    let route_state = connections.take_inbound_state_if(token, |_| true);
    let Some(route_state) = route_state else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let connection_token = connection.session_token();
            registered_connection(connections, &connection)?;
            if let Some(event) = connections
                .with_connection_mut(connection_token, |connection| {
                    connection.mark_cm_failure_into(
                        Error::Verbs(std::io::Error::other(message.clone())),
                        actions,
                    )
                })
                .flatten()
            {
                actions.push_event(event);
            }

            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection: EstablishedConnectionRoute::new(connection_token),
                    request: Some(request),
                    completion: Some(Error::Verbs(std::io::Error::other(message))),
                    selected: true,
                    reject: None,
                },
            );
            shared.begin_connection_close_into(connections, connection_token, io_core, actions);
            drop(connection);
            if connections
                .with_connection(connection_token, |connection| {
                    connection.io_ledger.accepted_count()
                })
                .unwrap_or(0)
                == 0
            {
                super::retirement::retire_registered_connection_into(
                    state,
                    connections,
                    shared,
                    io_core,
                    connection_token,
                    actions,
                )?;
            }
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => {
            let connection_token = connection.token;
            if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
                connections.release_route(token, true);
                return Ok(EventDisposition::Handled);
            }
            if let Some(event) = connections
                .with_connection_mut(connection_token, |connection| {
                    connection.mark_cm_failure_into(
                        Error::Verbs(std::io::Error::other(message.clone())),
                        actions,
                    )
                })
                .flatten()
            {
                actions.push_event(event);
            }
            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection: connection.clone(),
                    request: Some(Arc::clone(&request)),
                    completion: Some(Error::Verbs(std::io::Error::other(message.clone()))),
                    selected: true,
                    reject: None,
                },
            );
            if request.fail_undelivered_into(Error::Verbs(std::io::Error::other(message)), actions)
            {
                connections.set_inbound_state(
                    token,
                    InboundState::Closing {
                        connection: connection.clone(),
                        request: None,
                        completion: None,
                        selected: false,
                        reject: None,
                    },
                );
                if let Some(listener) = connections
                    .inbound_listener(token)
                    .and_then(|listener| listener.upgrade())
                    && listener.finish_selected_route(token.encode())
                {
                    state.enqueue_listener_work(&listener);
                }
            }
            shared.begin_connection_close_into(connections, connection_token, io_core, actions);
            if connections
                .with_connection(connection_token, |connection| {
                    connection.io_ledger.accepted_count()
                })
                .unwrap_or(0)
                == 0
            {
                super::retirement::retire_registered_connection_into(
                    state,
                    connections,
                    shared,
                    io_core,
                    connection_token,
                    actions,
                )?;
            }
        }
        InboundState::Established { connection } => {
            let connection_token = connection.token;
            if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
                connections.release_route(token, true);
                return Ok(EventDisposition::Handled);
            }
            if let Some(event) = connections
                .with_connection_mut(connection_token, |connection| {
                    connection.mark_cm_failure_into(
                        Error::Verbs(std::io::Error::other(message.clone())),
                        actions,
                    )
                })
                .flatten()
            {
                actions.push_event(event);
            }
            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection: connection.clone(),
                    request: None,
                    completion: None,
                    selected: false,
                    reject: None,
                },
            );
            shared.begin_connection_close_into(connections, connection_token, io_core, actions);
            if connections
                .with_connection(connection_token, |connection| {
                    connection.io_ledger.accepted_count()
                })
                .unwrap_or(0)
                == 0
            {
                super::retirement::retire_registered_connection_into(
                    state,
                    connections,
                    shared,
                    io_core,
                    connection_token,
                    actions,
                )?;
            }
        }
        route_state @ InboundState::Closing { .. } => {
            connections.set_inbound_state(token, route_state);
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        route_state @ InboundState::Quarantined { .. } => {
            connections.set_inbound_state(token, route_state);
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        route_state @ InboundState::PendingSelection { .. } => {
            connections.set_inbound_state(token, route_state);
            return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
        }
        InboundState::Transitioning => {
            return Err(Error::InvalidConfig(
                "inbound CM route was re-entered while transitioning".into(),
            ));
        }
    }
    Ok(EventDisposition::Handled)
}
