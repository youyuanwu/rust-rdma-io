//! Listener creation, inbound admission, acceptance, and CM transitions.

use std::sync::Arc;

use rdma_io_sys::rdmacm::rdma_cm_id;

use super::super::connection::ConnectionState;
use super::{
    AcceptRequest, ChildAdmission, CmEventReject, CmEventSnapshot, CmRouteToken, CmState,
    ContextRoute, EngineReactorResources, EstablishedConnectionRoute, EventDisposition,
    InboundPeerState, InboundRejectReason, InboundRoute, InboundState, IncomingChild,
    ListenRequest, ListenerAction, Lookup, RdmaConnection, RdmaListener, SessionManager,
    SharedCmId, VerbsConnectionResources, build_qp, contextual_cm_error,
    install_reserved_connection, is_failure_event, reserve_connection, run_setup_before_establish,
    with_validated_listener_backlog,
};
use crate::cm::{CmEventType, CmId, PortSpace};
#[cfg(test)]
use crate::v2::engine::registry::lock_unpoison;
use crate::v2::engine::registry::{ListenerToken, write_unpoison};
use crate::v2::engine::session::registry::{ConnectionRegistry, ConnectionSnapshot};
use crate::v2::error::{Error, Result};

pub(super) fn start_listener(
    state: &mut CmState,
    shared: &SessionManager,
    resources: &EngineReactorResources,
    token: ListenerToken,
    request: Arc<ListenRequest>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    if request.is_cancelled() || state.shutting_down || shared.shutdown_requested() {
        state.listeners.release(token, false);
        request.complete_into(Err(Error::DriverShutdown), actions);
        return Ok(());
    }
    let cm_id = match CmId::new_with_context_token(
        &resources.cm_event_channel,
        PortSpace::Tcp,
        token.encode(),
    ) {
        Ok(cm_id) => SharedCmId::new(cm_id, Arc::clone(&resources.cm_event_channel)),
        Err(error) => {
            state.listeners.release(token, false);
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
    let mut provider_backlog = 0;
    let listen_result = match with_validated_listener_backlog(&request.config, |backlog| {
        provider_backlog = backlog;
        cm_id.listen(&request.address, backlog)
    }) {
        Ok(result) => result,
        Err(error) => {
            state.defer_listener_cm_id(token, cm_id);
            request.complete_into(Err(error), actions);
            return Ok(());
        }
    };
    if let Err(error) = listen_result {
        state.defer_listener_cm_id(token, cm_id);
        request.complete_into(
            Err(contextual_cm_error(
                format!(
                    "listen on {} with backlog {}",
                    request.address, provider_backlog
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
            state.defer_listener_cm_id(token, cm_id);
            request.complete_into(Err(error), actions);
            return Ok(());
        }
    };
    let context_key = cm_id.context_key();
    let raw_id = cm_id.as_raw() as usize;
    if let Err(cm_id) =
        state.activate_listener_identity(token, local_addr, cm_id, raw_id, context_key)
    {
        state.defer_listener_cm_id(token, cm_id);
        request.complete_into(
            Err(Error::InvalidConfig(
                "duplicate listener route identity".into(),
            )),
            actions,
        );
        return Ok(());
    }
    if request.is_cancelled() {
        if state
            .listeners
            .get_mut(token)
            .is_some_and(|listener| listener.request_close())
        {
            state.enqueue_listener_work(token);
        }
        request.complete_into(Err(Error::DriverShutdown), actions);
    } else {
        let listener = RdmaListener::from_state(
            shared,
            state
                .listeners
                .get(token)
                .expect("new listener registry entry remains occupied"),
        );
        request.complete_into(Ok(listener), actions);
    }
    Ok(())
}

pub(super) fn service_listener(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: Option<&EngineReactorResources>,
    listener: ListenerToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let action = state
        .listeners
        .get_mut(listener)
        .map_or(ListenerAction::None, |listener| listener.next_action());
    match action {
        ListenerAction::CancelledBeforeSelection(request) => {
            request.complete_into(Err(Error::DriverShutdown), actions);
        }
        ListenerAction::FailUnselected(request) => {
            let error = state
                .listeners
                .get(listener)
                .map_or(Error::TransportClosed, |listener| listener.close_error());
            request.complete_into(Err(error), actions);
        }
        ListenerAction::RejectChild(child, reason) => {
            let result = reject_child(state, connections, child, reason);
            if let Some(listener) = state.listeners.get_mut(listener) {
                listener.release_unpaired_child_slot();
            }
            result?;
        }
        ListenerAction::ProcessSelected { request, child } => {
            let resources = resources.ok_or_else(|| {
                Error::InvalidConfig(
                    "selected listener setup requires live engine resources".into(),
                )
            })?;
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
            let result = reject_child(
                state,
                connections,
                child,
                InboundRejectReason::ListenerClosed,
            );
            request.complete_into(Err(reason), actions);
            if let Some(listener) = state.listeners.get_mut(listener) {
                listener.finish_selected_request(&request);
            }
            result?;
        }
        ListenerAction::CancelAfterAccept { request, route } => {
            let _ = request;
            cancel_inbound_route(state, connections, shared, io_core, route, actions)?;
        }
        ListenerAction::AcknowledgeDelivery { request, route } => {
            if !acknowledge_accept_delivery(state, connections, shared, listener, &request, route)?
            {
                if let Some(listener) = state.listeners.get_mut(listener) {
                    listener.mark_selected_route_closing(route);
                }
                cancel_inbound_route(state, connections, shared, io_core, route, actions)?;
            }
        }
        ListenerAction::FinalizeClose => {
            finalize_listener(state, listener)?;
        }
        ListenerAction::None => {}
    }
    Ok(())
}

fn acknowledge_accept_delivery(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    _shared: &SessionManager,
    listener: ListenerToken,
    request: &Arc<AcceptRequest>,
    encoded: u64,
) -> Result<bool> {
    let token = CmRouteToken::decode(encoded);
    let route_state = connections.take_inbound_state_if(token, |route_state| {
        matches!(
            route_state,
            InboundState::EstablishedAwaitingDelivery {
                request: current,
                ..
            } if Arc::ptr_eq(current, request)
        )
    });
    let acknowledged =
        if let Some(InboundState::EstablishedAwaitingDelivery { connection, .. }) = route_state {
            // `AcknowledgeDelivery` is produced only after the take-once
            // observer transferred the connection to the frontend. Listener
            // close or shutdown may race after that transfer, but cannot
            // reclaim the now user-owned connection.
            connections.set_inbound_state(token, InboundState::Established { connection });
            true
        } else if let Some(route_state) = route_state {
            connections.set_inbound_state(token, route_state);
            false
        } else {
            false
        };
    if acknowledged
        && state
            .listeners
            .get_mut(listener)
            .is_some_and(|listener| listener.finish_selected_route(encoded))
    {
        state.enqueue_listener_work(listener);
    }
    Ok(acknowledged)
}

pub(super) fn handle_connect_request(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    resources: &EngineReactorResources,
    listener: ListenerToken,
    snapshot: CmEventSnapshot,
) -> Result<EventDisposition> {
    let (listener_closing, listener_addr) = {
        let Some(listener) = state.listeners.get(listener) else {
            reject_raw_child(
                state,
                resources,
                snapshot.id,
                InboundRejectReason::ListenerClosed,
            )?;
            return Ok(EventDisposition::Handled);
        };
        (listener.is_closing(), listener.display_addr())
    };
    if snapshot.status != 0 || listener_closing {
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
            listener = %listener_addr,
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
        let mut route = InboundRoute::new(token, listener);
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
    let admitted = state.listeners.get_mut(listener).map_or(
        ChildAdmission {
            rejected: Some((
                IncomingChild::new(token),
                InboundRejectReason::ListenerClosed,
            )),
        },
        |listener| listener.admit_child(IncomingChild::new(token)),
    );
    drop(admission);
    if let Some((child, reason)) = admitted.rejected {
        reject_child(state, connections, child, reason)?;
    }
    let has_work = state
        .listeners
        .get(listener)
        .is_some_and(|listener| listener.has_work());
    if has_work {
        state.enqueue_listener_work(listener);
    }
    Ok(EventDisposition::Handled)
}

pub(super) fn handle_listener_event(
    state: &mut CmState,
    shared: &SessionManager,
    listener: ListenerToken,
    snapshot: CmEventSnapshot,
) -> Result<EventDisposition> {
    if !is_failure_event(snapshot.event_type) && snapshot.status == 0 {
        return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
    }
    let listener_addr = state.listeners.get(listener).map_or_else(
        || "<stale listener>".into(),
        |listener| listener.display_addr().to_string(),
    );
    let message = format!(
        "listener {} RDMA CM {:?} failed with status {} for id={:#x}",
        listener_addr, snapshot.event_type, snapshot.status, snapshot.id
    );
    if snapshot.event_type == CmEventType::DeviceRemoval {
        return Err(Error::Verbs(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            message,
        )));
    }
    let admission = write_unpoison(&shared.frontend.admission);
    let changed = state
        .listeners
        .get_mut(listener)
        .is_some_and(|listener| listener.fail(Error::Verbs(std::io::Error::other(message))));
    drop(admission);
    if changed {
        state.enqueue_listener_work(listener);
    }
    Ok(EventDisposition::Handled)
}

#[allow(
    clippy::too_many_arguments,
    reason = "CM transition dependencies stay explicit"
)]
fn process_selected_pair(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: &EngineReactorResources,
    listener: ListenerToken,
    request: Arc<AcceptRequest>,
    child: IncomingChild,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let (listener_closing, listener_error, listener_addr) = {
        let listener = state.listeners.get(listener);
        (
            listener.is_none_or(|listener| listener.is_closing()),
            listener.map_or(Error::TransportClosed, |listener| listener.close_error()),
            listener.map_or_else(
                || "<stale listener>".into(),
                |listener| listener.display_addr().to_string(),
            ),
        )
    };
    if request.is_cancelled() || listener_closing || shared.shutdown_requested() {
        let error = if listener_closing {
            listener_error
        } else {
            Error::DriverShutdown
        };
        let reject = if listener_closing || request.is_cancelled() {
            InboundRejectReason::ListenerClosed
        } else {
            InboundRejectReason::AdmissionClosed
        };
        reject_child(state, connections, child, reject)?;
        request.complete_into(Err(error), actions);
        if let Some(listener) = state.listeners.get_mut(listener) {
            listener.finish_selected_request(&request);
        }
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
        connections.set_inbound_state(
            token,
            InboundState::PendingSelection {
                cm_id: child_cm_id,
                reservation: child_reservation,
            },
        );
        reject_child(
            state,
            connections,
            IncomingChild::new(token),
            InboundRejectReason::SetupFailure,
        )?;
        request.complete_into(Err(error), actions);
        if let Some(listener) = state.listeners.get_mut(listener) {
            listener.finish_selected_request(&request);
        }
        return Ok(());
    }
    let raw_id = child_cm_id.as_raw() as usize;
    let context_key = child_cm_id.context_key();
    assert!(connections.set_route_identity(token, raw_id, context_key));
    if !state.insert_context_route(context_key, ContextRoute::Inbound { token, raw_id }) {
        connections.set_inbound_state(
            token,
            InboundState::PendingSelection {
                cm_id: child_cm_id,
                reservation: child_reservation,
            },
        );
        reject_child(
            state,
            connections,
            IncomingChild::new(token),
            InboundRejectReason::SetupFailure,
        )?;
        request.complete_into(
            Err(Error::InvalidConfig(
                "duplicate inbound CM context identity".into(),
            )),
            actions,
        );
        if let Some(listener) = state.listeners.get_mut(listener) {
            listener.finish_selected_request(&request);
        }
        return Ok(());
    }
    state
        .listeners
        .get_mut(listener)
        .ok_or(Error::TransportClosed)?
        .route_selected(&request, token.encode())?;

    let local_addr = child_cm_id.local_addr();
    let peer_addr = child_cm_id.peer_addr();
    let qp = match build_qp(resources, &child_cm_id, &config) {
        Ok(qp) => qp,
        Err(error) => {
            state.remove_owned_context_route(Some(&child_cm_id));
            connections.set_inbound_state(
                token,
                InboundState::PendingSelection {
                    cm_id: child_cm_id,
                    reservation: child_reservation,
                },
            );
            reject_child(
                state,
                connections,
                IncomingChild::new(token),
                InboundRejectReason::SetupFailure,
            )?;
            request.complete_into(
                Err(contextual_cm_error(
                    format!("build inbound QP for {listener_addr}"),
                    error,
                )),
                actions,
            );
            if let Some(listener) = state.listeners.get_mut(listener) {
                listener.finish_selected_route(token.encode());
            }
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
            if let Err(reject_error) =
                shared.reject_failed_connection_install(connections, &failed_resources)
            {
                let reject_error = contextual_cm_error(
                    "reject inbound child after failed connection installation",
                    reject_error,
                );
                let connection = state.retain_failed_install(
                    connections,
                    shared,
                    token,
                    failed_resources,
                    &reject_error,
                    actions,
                );
                connections.set_inbound_state(token, InboundState::Quarantined { connection });
                request.complete_into(Err(error), actions);
                if let Some(listener) = state.listeners.get_mut(listener) {
                    listener.fail(reject_error.clone());
                    listener.finish_selected_route(token.encode());
                }
                return Err(reject_error);
            }
            match shared.destroy_failed_connection_install(connections, &mut failed_resources) {
                Ok((cm_id, _qp_destroyed)) => {
                    state.release_failed_install(connections, failed_resources)?;
                    if let Some(cm_id) = cm_id {
                        state.defer_cm_id(cm_id);
                    }
                    connections.release_route(token, false);
                }
                Err(destroy_error) => {
                    CmState::record_setup_rollback_quarantine(&destroy_error);
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
            if let Some(listener) = state.listeners.get_mut(listener) {
                listener.finish_selected_route(token.encode());
            }
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
        actions,
        || {
            let listener_error = state
                .listeners
                .get(listener)
                .filter(|listener| listener.is_closing())
                .map(|listener| listener.close_error());
            if request.is_cancelled() || listener_error.is_some() || shared.shutdown_requested() {
                Err(if let Some(error) = listener_error {
                    error
                } else {
                    Error::DriverShutdown
                })
            } else {
                Ok(())
            }
        },
        |connections| {
            connections
                .with_connection_mut(connection.session_token(), |connection| {
                    connection.accept_inbound(&conn_param)
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
    state: &mut CmState,
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
    if connections
        .with_connection(connection_token, |connection| {
            connection.inbound_accept_succeeded()
        })
        .ok_or(Error::TransportClosed)?
    {
        return close_accepted_connection(
            state,
            connections,
            shared,
            io_core,
            token,
            request,
            EstablishedConnectionRoute::new(connection_token),
            Some(connection),
            error,
            actions,
        );
    }
    let reject = match &error {
        Error::DriverShutdown if !request.is_cancelled() => InboundRejectReason::AdmissionClosed,
        Error::DriverShutdown | Error::TransportClosed => InboundRejectReason::ListenerClosed,
        _ => InboundRejectReason::SetupFailure,
    };
    if let Err(reject_error) = connections
        .with_connection(connection_token, ConnectionState::reject)
        .ok_or(Error::TransportClosed)?
    {
        let reject_error = contextual_cm_error(
            format!("reject selected inbound child after setup failure ({reject:?})"),
            reject_error,
        );
        connections.set_inbound_state(
            token,
            InboundState::Quarantined {
                connection: Some(EstablishedConnectionRoute::new(connection_token)),
            },
        );
        shared.track_connection_quarantine(connections, connection_token);
        if let Some(event) = connections
            .with_connection_mut(connection_token, |connection| {
                connection
                    .publish_destroy_quarantine_into(&reject_error, || {}, actions)
                    .1
            })
            .flatten()
        {
            actions.push_event(event);
        }
        request.complete_into(Err(error), actions);
        if let Some(listener) = connections.inbound_listener(token)
            && state.listeners.get_mut(listener).is_some_and(|listener| {
                listener.fail(reject_error.clone());
                listener.finish_selected_route(token.encode())
            })
        {
            state.enqueue_listener_work(listener);
        }
        drop(connection);
        return Err(reject_error);
    }
    connections.set_inbound_state(
        token,
        InboundState::Closing {
            connection: EstablishedConnectionRoute::new(connection_token),
            request: Some(request),
            completion: Some(error),
            selected: true,
            peer: InboundPeerState::PreAccept { reject: None },
        },
    );
    shared.begin_connection_close_into(state, connections, connection_token, io_core, actions);
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
    state: &mut CmState,
    resources: &EngineReactorResources,
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
    state: &mut CmState,
    cm_id: SharedCmId,
    reason: InboundRejectReason,
) -> Result<()> {
    match cm_id.reject(&[]) {
        Ok(()) => {
            state.defer_cm_id(cm_id);
            Ok(())
        }
        Err(error) => {
            // A failed reject leaves the peer disposition uncertain. Keep the
            // exact CM ID and channel out of the ordinary destruction service;
            // driver failure retains the complete reactor quarantine.
            state.quarantine_cm_id(cm_id);
            Err(contextual_cm_error(
                format!("reject inbound child ({reason:?})"),
                Error::from_v1(error),
            ))
        }
    }
}

fn reject_child(
    state: &mut CmState,
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
    state: &mut CmState,
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
            .and_then(|listener| {
                state
                    .listeners
                    .get(listener)
                    .filter(|entry| entry.is_closing())
                    .map(|entry| entry.close_error())
            })
            .unwrap_or(Error::DriverShutdown)
    };
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let error = cancellation_error();
            let connection_token = connection.session_token();
            registered_connection(connections, &connection)?;
            close_accepted_connection(
                state,
                connections,
                shared,
                io_core,
                token,
                request,
                EstablishedConnectionRoute::new(connection_token),
                Some(connection),
                error,
                actions,
            )?;
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => {
            let error = cancellation_error();
            close_accepted_connection(
                state,
                connections,
                shared,
                io_core,
                token,
                request,
                connection,
                None,
                error,
                actions,
            )?;
        }
        _ => unreachable!("inbound cancellation state was pre-filtered"),
    }
    Ok(())
}

pub(super) fn prepare_selected_connection_close(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    encoded: u64,
    connection_token: crate::v2::engine::registry::ConnectionToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<bool> {
    let accepted = connections
        .with_connection(connection_token, |connection| {
            connection.inbound_accept_succeeded()
        })
        .ok_or(Error::TransportClosed)?;
    if !accepted {
        return Ok(true);
    }
    let token = CmRouteToken::decode(encoded);
    let route_state = connections.take_inbound_state_if(token, |route_state| {
        matches!(
            route_state,
            InboundState::AwaitEstablished { connection, .. }
                if connection.session_token() == connection_token
        ) || matches!(
            route_state,
            InboundState::EstablishedAwaitingDelivery { connection, .. }
                if connection.token == connection_token
        ) || matches!(
            route_state,
            InboundState::Established { connection }
                if connection.token == connection_token
        )
    });
    let (request, connection, route_frontend) = match route_state {
        Some(InboundState::AwaitEstablished {
            request,
            connection,
        }) => (
            request,
            EstablishedConnectionRoute::new(connection_token),
            Some(connection),
        ),
        Some(InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        }) => (request, connection, None),
        Some(InboundState::Established { connection }) => {
            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection,
                    request: None,
                    completion: None,
                    selected: false,
                    peer: InboundPeerState::Accepted,
                },
            );
            return Ok(true);
        }
        Some(route_state) => {
            connections.set_inbound_state(token, route_state);
            return Ok(true);
        }
        None => return Ok(true),
    };
    let error = connections
        .inbound_listener(token)
        .and_then(|listener| {
            state
                .listeners
                .get(listener)
                .filter(|listener| listener.is_closing())
                .map(|listener| listener.close_error())
        })
        .unwrap_or(Error::DriverShutdown);
    let failure = request.claim_undelivered_failure_into(error, actions);
    connections.set_inbound_state(
        token,
        InboundState::Closing {
            connection,
            request: None,
            completion: None,
            selected: true,
            peer: InboundPeerState::Accepted,
        },
    );
    let observer_frontend = failure.into_connection();
    drop(route_frontend);
    drop(observer_frontend);
    Ok(true)
}

#[allow(
    clippy::too_many_arguments,
    reason = "accepted-route cancellation keeps reactor-owned transition dependencies explicit"
)]
fn close_accepted_connection(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    request: Arc<AcceptRequest>,
    connection: EstablishedConnectionRoute,
    frontend_connection: Option<RdmaConnection>,
    error: Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let connection_token = connection.token;
    let accepted = connections
        .with_connection(connection_token, |connection| {
            connection.inbound_accept_succeeded()
        })
        .ok_or(Error::TransportClosed)?;
    if !accepted {
        return Err(Error::InvalidConfig(
            "accepted inbound teardown lacks successful provider accept evidence".into(),
        ));
    }
    let failure = request.claim_undelivered_failure_into(error, actions);
    if failure.delivered() {
        connections.set_inbound_state(
            token,
            InboundState::Established {
                connection: connection.clone(),
            },
        );
        if let Some(listener) = connections.inbound_listener(token)
            && state
                .listeners
                .get_mut(listener)
                .is_some_and(|listener| listener.finish_selected_route(token.encode()))
        {
            state.enqueue_listener_work(listener);
        }
        drop(frontend_connection);
        drop(failure.into_connection());
        return Ok(());
    }
    connections.set_inbound_state(
        token,
        InboundState::Closing {
            connection,
            request: None,
            completion: None,
            selected: true,
            peer: InboundPeerState::Accepted,
        },
    );
    let cancelled_connection = failure.into_connection();
    shared.begin_connection_close_into(state, connections, connection_token, io_core, actions);
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
    drop(frontend_connection);
    drop(cancelled_connection);
    Ok(())
}

fn finalize_listener(state: &mut CmState, listener: ListenerToken) -> Result<()> {
    let (cm_id, raw_id, context_key, destruction_pending) = {
        let Some(entry) = state.listeners.get_mut(listener) else {
            return Ok(());
        };
        let raw_id = entry.raw_id();
        let context_key = entry.context_key();
        let destruction_pending = entry.cm_destruction_pending();
        let cm_id = entry.take_cm_id();
        if raw_id != 0 {
            state.listeners.remove_identity(listener, raw_id);
        }
        (cm_id, raw_id, context_key, destruction_pending)
    };
    let Some(cm_id) = cm_id else {
        if destruction_pending {
            return Ok(());
        }
        if raw_id != 0 && context_key != 0 {
            state.remove_context_route_if_owned(context_key, raw_id, Some(listener.encode()));
        }
        state.listeners.release(listener, true);
        return Ok(());
    };
    state.defer_listener_cm_id(listener, cm_id);
    Ok(())
}

pub(super) fn handle_event(
    state: &mut CmState,
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
            let listener = connections.inbound_listener(token);
            let listener_closed = listener.is_none_or(|listener| {
                state
                    .listeners
                    .get(listener)
                    .is_none_or(|listener| listener.is_closing())
            });
            if request.is_cancelled() || listener_closed || shared.shutdown_requested() {
                let error = if request.is_cancelled() {
                    Error::DriverShutdown
                } else if let Some(listener) = listener {
                    state
                        .listeners
                        .get(listener)
                        .map_or(Error::DriverShutdown, |listener| listener.close_error())
                } else {
                    Error::DriverShutdown
                };
                let connection_token = connection.session_token();
                registered_connection(connections, &connection)?;
                close_accepted_connection(
                    state,
                    connections,
                    shared,
                    io_core,
                    token,
                    request,
                    EstablishedConnectionRoute::new(connection_token),
                    Some(connection),
                    error,
                    actions,
                )?;
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
        CmEventType::TimewaitExit => {
            let closing = connections.take_inbound_state_if(token, |route_state| {
                matches!(route_state, InboundState::Closing { .. })
            });
            if let Some(closing) = closing {
                finish_closing_terminal(
                    state,
                    connections,
                    shared,
                    io_core,
                    token,
                    closing,
                    actions,
                )
            } else {
                Ok(EventDisposition::Handled)
            }
        }
        _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
    }
}

pub(super) fn handle_disconnected(
    state: &mut CmState,
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
                | InboundState::Closing { .. }
        )
    });
    let Some(route_state) = route_state else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    let disconnect_error = Error::Verbs(std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "inbound connection disconnected before accept delivery",
    ));
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let connection_token = connection.session_token();
            registered_connection(connections, &connection)?;
            close_after_inbound_terminal(
                state,
                connections,
                shared,
                io_core,
                token,
                EstablishedConnectionRoute::new(connection_token),
                Some(connection),
                Some(request),
                true,
                disconnect_error,
                false,
                actions,
            )
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => close_after_inbound_terminal(
            state,
            connections,
            shared,
            io_core,
            token,
            connection,
            None,
            Some(request),
            true,
            disconnect_error,
            false,
            actions,
        ),
        InboundState::Established { connection } => close_after_inbound_terminal(
            state,
            connections,
            shared,
            io_core,
            token,
            connection,
            None,
            None,
            false,
            disconnect_error,
            false,
            actions,
        ),
        route_state @ InboundState::Closing { .. } => finish_closing_terminal(
            state,
            connections,
            shared,
            io_core,
            token,
            route_state,
            actions,
        ),
        _ => unreachable!("inbound disconnect state was pre-filtered"),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "terminal inbound close keeps route, request, and publication ownership explicit"
)]
fn close_after_inbound_terminal(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    connection: EstablishedConnectionRoute,
    frontend_connection: Option<RdmaConnection>,
    request: Option<Arc<AcceptRequest>>,
    selected: bool,
    error: Error,
    record_failure: bool,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let connection_token = connection.token;
    let accepted = connections
        .with_connection(connection_token, |connection| {
            connection.inbound_accept_succeeded()
        })
        .ok_or_else(|| {
            Error::InvalidConfig(
                "terminal inbound event lost its registered connection owner".into(),
            )
        })?;
    if !accepted {
        return Err(Error::InvalidConfig(
            "terminal inbound event lacks successful provider accept evidence".into(),
        ));
    }
    let event = if record_failure {
        connections
            .with_connection_mut(connection_token, |connection| {
                connection.mark_cm_failure_into(error.clone(), actions)
            })
            .flatten()
    } else {
        connections
            .with_connection_mut(connection_token, ConnectionState::mark_disconnected)
            .flatten()
    };
    if let Some(event) = event {
        actions.push_event(event);
    }
    let observer_connection = request
        .map(|request| request.claim_undelivered_failure_into(error, actions))
        .and_then(|failure| failure.into_connection());
    connections.set_inbound_state(
        token,
        InboundState::Closing {
            connection,
            request: None,
            completion: None,
            selected,
            peer: InboundPeerState::Accepted,
        },
    );
    shared.begin_connection_close_into(state, connections, connection_token, io_core, actions);
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
    drop(frontend_connection);
    drop(observer_connection);
    Ok(EventDisposition::Handled)
}

fn finish_closing_terminal(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    route_state: InboundState,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let InboundState::Closing {
        connection,
        request,
        completion,
        selected,
        peer,
    } = route_state
    else {
        unreachable!("closing terminal helper requires a closing route");
    };
    let peer = match peer {
        InboundPeerState::PreAccept {
            reject: Some(_reason),
        } => InboundPeerState::PreAccept { reject: None },
        peer @ (InboundPeerState::PreAccept { reject: None } | InboundPeerState::Accepted) => {
            connections.set_inbound_state(
                token,
                InboundState::Closing {
                    connection,
                    request,
                    completion,
                    selected,
                    peer,
                },
            );
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
    };
    let connection_token = connection.token;
    if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
        return Err(Error::InvalidConfig(
            "closing inbound terminal event lost its registered connection owner".into(),
        ));
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
            connection,
            request,
            completion,
            selected,
            peer,
        },
    );
    shared.begin_connection_close_into(state, connections, connection_token, io_core, actions);
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
    state: &mut CmState,
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
    let failure = Error::Verbs(std::io::Error::other(message));
    match route_state {
        InboundState::AwaitEstablished {
            request,
            connection,
        } => {
            let connection_token = connection.session_token();
            registered_connection(connections, &connection)?;
            close_after_inbound_terminal(
                state,
                connections,
                shared,
                io_core,
                token,
                EstablishedConnectionRoute::new(connection_token),
                Some(connection),
                Some(request),
                true,
                failure,
                true,
                actions,
            )
        }
        InboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => close_after_inbound_terminal(
            state,
            connections,
            shared,
            io_core,
            token,
            connection,
            None,
            Some(request),
            true,
            failure,
            true,
            actions,
        ),
        InboundState::Established { connection } => close_after_inbound_terminal(
            state,
            connections,
            shared,
            io_core,
            token,
            connection,
            None,
            None,
            false,
            failure,
            true,
            actions,
        ),
        route_state @ InboundState::Closing { .. } => finish_closing_terminal(
            state,
            connections,
            shared,
            io_core,
            token,
            route_state,
            actions,
        ),
        route_state @ InboundState::Quarantined { .. } => {
            connections.set_inbound_state(token, route_state);
            Ok(EventDisposition::Rejected(CmEventReject::Duplicate))
        }
        route_state @ InboundState::PendingSelection { .. } => {
            connections.set_inbound_state(token, route_state);
            Ok(EventDisposition::Rejected(CmEventReject::Unexpected))
        }
        InboundState::Transitioning => Err(Error::InvalidConfig(
            "inbound CM route was re-entered while transitioning".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::v2::engine::CompletionMode;
    use crate::v2::engine::RdmaConnectionConfig;
    use crate::v2::engine::session::connection::TestConnectionProvider;
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct OrderedClosePoster {
        qp_num: u32,
        steps: Mutex<Vec<&'static str>>,
    }

    impl OrderedClosePoster {
        fn new(qp_num: u32) -> Arc<Self> {
            Arc::new(Self {
                qp_num,
                steps: Mutex::new(Vec::new()),
            })
        }

        fn steps(&self) -> Vec<&'static str> {
            self.steps
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl TestConnectionProvider for OrderedClosePoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn to_error(&self) -> Result<()> {
            lock_unpoison(&self.steps).push("to_error");
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            lock_unpoison(&self.steps).push("destroy_qp");
            Ok(true)
        }

        fn disconnect(&self) -> Result<()> {
            lock_unpoison(&self.steps).push("disconnect");
            Ok(())
        }
    }

    fn install_selected_route(
        manager: &SessionManager,
        state: &mut CmState,
        connections: &mut ConnectionRegistry,
        poster: Arc<OrderedClosePoster>,
    ) -> (
        RdmaListener,
        ListenerToken,
        Arc<AcceptRequest>,
        CmRouteToken,
        RdmaConnection,
    ) {
        let (listener, listener_token) = state.test_listener(manager, 1);
        let request = state
            .listeners
            .get(listener_token)
            .expect("test listener remains registered")
            .test_accept_request();
        let reservation = connections
            .try_reserve()
            .expect("test connection admission is available");
        let route = connections
            .register_inbound(|route| InboundRoute::new(route, listener_token))
            .expect("test inbound route is available");
        let connection = install_reserved_connection(
            manager,
            connections,
            Some(route),
            poster,
            RdmaConnectionConfig::default(),
            None,
            None,
            reservation,
        )
        .expect("test inbound connection installs");
        connections
            .with_connection_mut(route, |connection| {
                connection.record_inbound_accept_for_test();
            })
            .expect("test inbound connection remains registered");
        let listener_entry = state
            .listeners
            .get_mut(listener_token)
            .expect("test listener remains registered");
        listener_entry
            .register_waiter(Arc::clone(&request))
            .unwrap();
        assert!(
            listener_entry
                .admit_child(IncomingChild::new(route))
                .rejected
                .is_none()
        );
        assert!(matches!(
            listener_entry.next_action(),
            ListenerAction::ProcessSelected { .. }
        ));
        listener_entry
            .route_selected(&request, route.encode())
            .unwrap();
        (listener, listener_token, request, route, connection)
    }

    fn assert_selected_route_released(
        state: &CmState,
        connections: &ConnectionRegistry,
        listener: ListenerToken,
        route: CmRouteToken,
    ) {
        assert!(!matches!(
            connections.lookup_inbound(route),
            Lookup::Occupied(_)
        ));
        let listener = state
            .listeners
            .get(listener)
            .expect("test listener remains registered");
        assert!(!listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 0);
        assert_eq!(listener.admission().available_permits(), 1);
    }

    #[test]
    fn cancellation_after_provider_accept_runs_ordered_close_immediately() {
        let (_engine, mut driver) = crate::v2::engine::test_engine_pair(CompletionMode::Polling);
        let poster = OrderedClosePoster::new(71);
        let (listener, listener_token, request, route, connection) = install_selected_route(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster),
        );
        assert!(driver.reactor.session.connections.set_inbound_state(
            route,
            InboundState::AwaitEstablished {
                request: Arc::clone(&request),
                connection,
            },
        ));
        request.cancel();

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        cancel_inbound_route(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            &driver.reactor.session.manager,
            driver.reactor.io.core_mut(),
            route.encode(),
            &mut actions,
        )
        .unwrap();
        assert_eq!(poster.steps(), vec!["to_error", "destroy_qp"]);
        assert!(matches!(
            request.take_result_for_test(),
            Some(Err(Error::DriverShutdown))
        ));
        assert_selected_route_released(
            &driver.reactor.session.cm,
            &driver.reactor.session.connections,
            listener_token,
            route,
        );
        actions.publish();
        driver
            .reactor
            .session
            .cm
            .release_test_listener(listener_token);
        drop(listener);
    }

    #[test]
    fn cancellation_after_accept_success_wake_runs_ordered_close_immediately() {
        let (_engine, mut driver) = crate::v2::engine::test_engine_pair(CompletionMode::Polling);
        let poster = OrderedClosePoster::new(72);
        let (listener, listener_token, request, route, connection) = install_selected_route(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster),
        );
        assert!(driver.reactor.session.connections.set_inbound_state(
            route,
            InboundState::EstablishedAwaitingDelivery {
                request: Arc::clone(&request),
                connection: EstablishedConnectionRoute::new(route),
            },
        ));
        assert!(driver.reactor.session.connections.mark_active(route));
        let mut publication = crate::v2::engine::reactor::ReactorActions::default();
        request.complete_success_into(connection, &mut publication);
        publication.publish();
        request.cancel();
        assert!(request.has_undelivered_success_for_test());

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        cancel_inbound_route(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            &driver.reactor.session.manager,
            driver.reactor.io.core_mut(),
            route.encode(),
            &mut actions,
        )
        .unwrap();
        assert_eq!(poster.steps(), vec!["to_error", "destroy_qp"]);
        assert!(matches!(
            request.take_result_for_test(),
            Some(Err(Error::DriverShutdown))
        ));
        assert_selected_route_released(
            &driver.reactor.session.cm,
            &driver.reactor.session.connections,
            listener_token,
            route,
        );
        actions.publish();
        driver
            .reactor
            .session
            .cm
            .release_test_listener(listener_token);
        drop(listener);
    }

    #[test]
    fn listener_close_after_frontend_consumes_success_releases_selection() {
        let (_engine, mut driver) = crate::v2::engine::test_engine_pair(CompletionMode::Polling);
        let poster = OrderedClosePoster::new(73);
        let (listener, listener_token, request, route, connection) = install_selected_route(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster),
        );
        assert!(driver.reactor.session.connections.set_inbound_state(
            route,
            InboundState::EstablishedAwaitingDelivery {
                request: Arc::clone(&request),
                connection: EstablishedConnectionRoute::new(route),
            },
        ));
        assert!(driver.reactor.session.connections.mark_active(route));
        let mut publication = crate::v2::engine::reactor::ReactorActions::default();
        request.complete_success_into(connection, &mut publication);
        publication.publish();
        let delivered = request
            .take_result_for_test()
            .expect("published accept result exists")
            .expect("published accept succeeded");
        drop(delivered);
        assert!(
            driver
                .reactor
                .session
                .cm
                .listeners
                .get_mut(listener_token)
                .expect("test listener remains registered")
                .request_close()
        );

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        assert!(
            acknowledge_accept_delivery(
                &mut driver.reactor.session.cm,
                &mut driver.reactor.session.connections,
                &driver.reactor.session.manager,
                listener_token,
                &request,
                route.encode(),
            )
            .unwrap()
        );
        {
            let listener = driver
                .reactor
                .session
                .cm
                .listeners
                .get(listener_token)
                .expect("test listener remains registered");
            assert!(!listener.selected_is_some());
            assert_eq!(listener.child_slots_used(), 0);
            assert_eq!(listener.admission().available_permits(), 1);
        }
        driver.reactor.session.manager.begin_connection_close_into(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            route,
            driver.reactor.io.core_mut(),
            &mut actions,
        );
        super::super::retirement::retire_registered_connection_into(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            &driver.reactor.session.manager,
            driver.reactor.io.core_mut(),
            route,
            &mut actions,
        )
        .unwrap();
        assert_eq!(poster.steps(), vec!["to_error", "destroy_qp"]);
        assert_selected_route_released(
            &driver.reactor.session.cm,
            &driver.reactor.session.connections,
            listener_token,
            route,
        );
        actions.publish();
        driver
            .reactor
            .session
            .cm
            .release_test_listener(listener_token);
        drop(listener);
    }

    #[test]
    fn frontend_close_after_delivery_arms_listener_wait_once() {
        let (_engine, mut driver) = crate::v2::engine::test_engine_pair(CompletionMode::Polling);
        let poster = OrderedClosePoster::new(75);
        let (listener, listener_token, request, route, connection) = install_selected_route(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster),
        );
        assert!(driver.reactor.session.connections.set_inbound_state(
            route,
            InboundState::EstablishedAwaitingDelivery {
                request: Arc::clone(&request),
                connection: EstablishedConnectionRoute::new(route),
            },
        ));
        assert!(driver.reactor.session.connections.mark_active(route));
        let mut publication = crate::v2::engine::reactor::ReactorActions::default();
        request.complete_success_into(connection, &mut publication);
        publication.publish();
        let delivered = request
            .take_result_for_test()
            .expect("published accept result exists")
            .expect("published accept succeeded");
        drop(delivered);

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        driver.reactor.session.manager.begin_connection_close_into(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            route,
            driver.reactor.io.core_mut(),
            &mut actions,
        );
        super::super::retirement::retire_registered_connection_into(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            &driver.reactor.session.manager,
            driver.reactor.io.core_mut(),
            route,
            &mut actions,
        )
        .unwrap();
        service_listener(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            &driver.reactor.session.manager,
            driver.reactor.io.core_mut(),
            None,
            listener_token,
            &mut actions,
        )
        .unwrap();

        assert_eq!(poster.steps(), vec!["to_error", "destroy_qp"]);
        assert_selected_route_released(
            &driver.reactor.session.cm,
            &driver.reactor.session.connections,
            listener_token,
            route,
        );
        assert!(
            !driver
                .reactor
                .session
                .cm
                .listeners
                .get(listener_token)
                .expect("test listener remains registered")
                .has_work()
        );
        actions.publish();
        driver
            .reactor
            .session
            .cm
            .release_test_listener(listener_token);
        drop(listener);
    }

    #[test]
    fn accepted_setup_rollback_runs_ordered_close_without_reject_disposition() {
        let (_engine, mut driver) = crate::v2::engine::test_engine_pair(CompletionMode::Polling);
        let poster = OrderedClosePoster::new(74);
        let (listener, listener_token, request, route, connection) = install_selected_route(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            Arc::clone(&poster),
        );
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        fail_selected_connection(
            &mut driver.reactor.session.cm,
            &mut driver.reactor.session.connections,
            &driver.reactor.session.manager,
            driver.reactor.io.core_mut(),
            route,
            Arc::clone(&request),
            connection,
            Error::TransportClosed,
            &mut actions,
        )
        .unwrap();

        assert_eq!(poster.steps(), vec!["to_error", "destroy_qp"]);
        assert!(matches!(
            request.take_result_for_test(),
            Some(Err(Error::TransportClosed))
        ));
        assert_selected_route_released(
            &driver.reactor.session.cm,
            &driver.reactor.session.connections,
            listener_token,
            route,
        );
        actions.publish();
        driver
            .reactor
            .session
            .cm
            .release_test_listener(listener_token);
        drop(listener);
    }
}
