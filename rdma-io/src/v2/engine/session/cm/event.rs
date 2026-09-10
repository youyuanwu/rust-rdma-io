//! CM event acquisition, acknowledgement, and exact route dispatch.

#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::Ordering;

use super::{
    CmState, ContextRoute, EngineReactorResources, InboundRejectReason, Lookup, SessionManager,
};
use crate::cm::CmEventType;
use crate::v2::engine::registry::{ConnectionToken, ListenerToken};
use crate::v2::engine::session::registry::{ConnectionRegistry, ConnectionRouteIdentity};
use crate::v2::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CmEventReject {
    Stale,
    Duplicate,
    Unknown,
    WrongId,
    Unexpected,
}

#[derive(Clone, Copy)]
pub(super) struct CmEventSnapshot {
    pub(super) event_type: CmEventType,
    pub(super) status: i32,
    pub(super) id: usize,
    pub(super) listen_id: usize,
    pub(super) context_key: usize,
}

pub(super) enum EventDisposition {
    Handled,
    IgnoredAfterShutdown,
    Rejected(CmEventReject),
}

pub(super) enum CmDispatchRoute {
    Outbound(ConnectionToken),
    Inbound(ConnectionToken),
    Listener(ListenerToken),
}

pub(super) struct PendingCmEvent {
    snapshot: CmEventSnapshot,
    route: std::result::Result<CmDispatchRoute, CmEventReject>,
}

pub(super) fn acquire_event(
    state: &CmState,
    connections: &ConnectionRegistry,
    resources: &EngineReactorResources,
) -> Result<Option<PendingCmEvent>> {
    let event = match resources.cm_event_channel.try_get_event() {
        Ok(event) => event,
        Err(crate::Error::WouldBlock) => return Ok(None),
        Err(crate::Error::Verbs(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(None);
        }
        Err(error) => return Err(Error::from_v1(error)),
    };
    let snapshot = CmEventSnapshot {
        event_type: event.event_type(),
        status: event.status(),
        id: event.cm_id_raw() as usize,
        listen_id: event.listen_id_raw() as usize,
        context_key: event.context_key(),
    };
    let route = lookup_dispatch_route(state, connections, snapshot);
    event.ack_checked().map_err(Error::from_v1)?;
    Ok(Some(PendingCmEvent { snapshot, route }))
}

pub(super) fn try_process_event(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: &EngineReactorResources,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<bool> {
    let pending = if let Some(pending) = state.take_pending_event() {
        pending
    } else {
        let Some(pending) = acquire_event(state, connections, resources)? else {
            return Ok(false);
        };
        pending
    };
    process_event(
        state,
        connections,
        shared,
        io_core,
        resources,
        pending,
        actions,
    )?;
    Ok(true)
}

fn process_event(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: &EngineReactorResources,
    pending: PendingCmEvent,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let PendingCmEvent { snapshot, route } = pending;
    let route = match route {
        Ok(route) => route,
        Err(reject) => {
            record_cm_reject(shared, reject);
            if snapshot.event_type == CmEventType::ConnectRequest {
                state.reject_raw_child(
                    resources,
                    snapshot.id,
                    InboundRejectReason::ListenerClosed,
                )?;
            }
            return Ok(());
        }
    };
    let disposition = match route {
        CmDispatchRoute::Outbound(token) => state.handle_event(
            connections,
            shared,
            io_core,
            resources,
            token,
            snapshot,
            actions,
        )?,
        CmDispatchRoute::Inbound(token) => {
            state.handle_inbound_event(connections, shared, io_core, token, snapshot, actions)?
        }
        CmDispatchRoute::Listener(listener) => {
            if snapshot.event_type == CmEventType::ConnectRequest {
                state.handle_connect_request(connections, shared, resources, listener, snapshot)?
            } else {
                state.handle_listener_event(shared, listener, snapshot)?
            }
        }
    };
    match disposition {
        EventDisposition::Handled | EventDisposition::IgnoredAfterShutdown => {}
        EventDisposition::Rejected(reject) => record_cm_reject(shared, reject),
    }
    Ok(())
}

pub(super) fn lookup_dispatch_route(
    state: &CmState,
    connections: &ConnectionRegistry,
    snapshot: CmEventSnapshot,
) -> std::result::Result<CmDispatchRoute, CmEventReject> {
    if snapshot.event_type == CmEventType::ConnectRequest {
        let token = state
            .listeners
            .token_for_raw(snapshot.listen_id)
            .ok_or(CmEventReject::Unknown)?;
        let listener = match state.listeners.lookup(token) {
            Lookup::Occupied(listener) => listener,
            Lookup::Duplicate => return Err(CmEventReject::Duplicate),
            Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
            Lookup::Unknown => return Err(CmEventReject::Unknown),
        };
        if listener.raw_id() != snapshot.listen_id {
            return Err(CmEventReject::WrongId);
        }
        return Ok(CmDispatchRoute::Listener(token));
    }
    if snapshot.context_key == 0 {
        return Err(CmEventReject::Unknown);
    }
    let route = state
        .context_routes
        .get(&snapshot.context_key)
        .copied()
        .ok_or(CmEventReject::Unknown)?;
    match route {
        ContextRoute::Outbound { .. } => lookup_event_route(state, connections, snapshot)
            .map(|route| CmDispatchRoute::Outbound(route.token)),
        ContextRoute::Inbound { token, raw_id } => {
            if raw_id != snapshot.id {
                return Err(CmEventReject::WrongId);
            }
            let route = match connections.lookup_inbound(token) {
                Lookup::Occupied(route) => route,
                Lookup::Duplicate => return Err(CmEventReject::Duplicate),
                Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
                Lookup::Unknown => return Err(CmEventReject::Unknown),
            };
            if route.raw_id != raw_id || route.context_key != snapshot.context_key {
                return Err(CmEventReject::WrongId);
            }
            Ok(CmDispatchRoute::Inbound(token))
        }
        ContextRoute::Listener { token, raw_id } => {
            if raw_id != snapshot.id {
                return Err(CmEventReject::WrongId);
            }
            let listener = match state.listeners.lookup(token) {
                Lookup::Occupied(listener) => listener,
                Lookup::Duplicate => return Err(CmEventReject::Duplicate),
                Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
                Lookup::Unknown => return Err(CmEventReject::Unknown),
            };
            if listener.raw_id() != raw_id || listener.context_key() != snapshot.context_key {
                return Err(CmEventReject::WrongId);
            }
            Ok(CmDispatchRoute::Listener(token))
        }
    }
}

pub(super) fn lookup_event_route(
    state: &CmState,
    connections: &ConnectionRegistry,
    snapshot: CmEventSnapshot,
) -> std::result::Result<ConnectionRouteIdentity, CmEventReject> {
    if snapshot.context_key == 0 {
        return Err(CmEventReject::Unknown);
    }
    let route = state
        .context_routes
        .get(&snapshot.context_key)
        .copied()
        .ok_or(CmEventReject::Unknown)?;
    let ContextRoute::Outbound { token, raw_id } = route else {
        return Err(CmEventReject::Unexpected);
    };
    if raw_id != snapshot.id {
        return Err(CmEventReject::WrongId);
    }
    let route = match connections.lookup_outbound(token) {
        Lookup::Occupied(route) => route,
        Lookup::Duplicate => return Err(CmEventReject::Duplicate),
        Lookup::Stale | Lookup::Retired => return Err(CmEventReject::Stale),
        Lookup::Unknown => return Err(CmEventReject::Unknown),
    };
    if route.raw_id != raw_id || route.context_key != snapshot.context_key {
        return Err(CmEventReject::WrongId);
    }
    Ok(route)
}

fn record_cm_reject(manager: &SessionManager, reject: CmEventReject) {
    #[cfg(any(test, feature = "test-hooks"))]
    if !matches!(reject, CmEventReject::Duplicate) {
        manager.rejected_cm_events.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(not(any(test, feature = "test-hooks")))]
    let _ = (manager, reject);
}

pub(super) fn is_failure_event(event: CmEventType) -> bool {
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
