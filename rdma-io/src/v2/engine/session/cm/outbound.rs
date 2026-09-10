//! Outbound connection setup, delivery, cancellation, and CM transitions.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use super::super::SessionReactorSources;
use super::super::connection::ConnectionState;
use super::{
    CmEventReject, CmEventSnapshot, CmRouteToken, CmState, ConnectionSetup, ContextRoute,
    EngineReactorResources, EstablishedConnectionRoute, EventDisposition, Lookup, OutboundRoute,
    OutboundState, RdmaConnection, RdmaConnectionConfig, SessionContext, SessionFrontend,
    SharedCmId, VerbsConnectionResources, build_qp, empty_connection_setup,
    install_reserved_connection, is_failure_event, run_setup_before_establish,
};
use crate::cm::{CmEventType, CmId, PortSpace};
use crate::v2::engine::reactor::CommandIngress;
use crate::v2::engine::reactor::completion::CommandCompletion;
use crate::v2::engine::registry::lock_unpoison;
use crate::v2::engine::session::registry::ConnectionRegistry;
use crate::v2::error::{Error, Result};

pub(in crate::v2::engine) async fn connect(
    shared: Arc<SessionFrontend>,
    commands: Arc<CommandIngress>,
    address: SocketAddr,
    config: RdmaConnectionConfig,
) -> Result<RdmaConnection> {
    connect_with_setup(shared, commands, address, config, empty_connection_setup()).await
}

pub(in crate::v2::engine) async fn connect_with_setup(
    shared: Arc<SessionFrontend>,
    commands: Arc<CommandIngress>,
    address: SocketAddr,
    config: RdmaConnectionConfig,
    setup: ConnectionSetup,
) -> Result<RdmaConnection> {
    shared.validate_connection_config(&config)?;
    let lane = commands
        .acquire_connect()
        .await
        .ok_or_else(|| shared.admission_error().unwrap_or(Error::DriverShutdown))?;
    let permit = commands.reserve_connect(lane).map_err(|error| {
        if matches!(error, Error::DriverShutdown) {
            shared.admission_error().unwrap_or(error)
        } else {
            error
        }
    })?;
    let admission = super::super::super::registry::read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let request = Arc::new(OutboundRequest::new(address, config, setup));
    #[cfg(any(test, feature = "test-hooks"))]
    shared.pause_connect_before_enqueue();
    commands.enqueue_connect(Arc::clone(&request), permit);
    drop(admission);
    commands.notify_reactor();
    let waiter = ConnectWaiter {
        commands: Arc::downgrade(&commands),
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    };
    drop(request);
    drop(shared);
    CommandIngress::yield_after_admission().await;
    waiter.await
}

pub(super) fn start(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    resources: &EngineReactorResources,
    request: Arc<OutboundRequest>,
    reservation: super::ConnectionReservation,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<bool> {
    if request.observer.completion.is_cancelled() || state.shutting_down {
        drop(reservation);
        request.complete_into(Err(Error::DriverShutdown), actions);
        return Ok(false);
    }
    let token = match connections
        .register_outbound(|token| OutboundRoute::new(token, Arc::clone(&request)))
    {
        Ok(token) => token,
        Err(error) => {
            drop(reservation);
            request.complete_failure_into(error, actions);
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
            connections.release_route(token, false);
            drop(reservation);
            request.complete_failure_into(Error::from_v1(error), actions);
            return Ok(false);
        }
    };
    let Some(context_token) = cm_id.context_token() else {
        state.defer_cm_id(cm_id);
        connections.release_route(token, false);
        drop(reservation);
        request.complete_failure_into(
            Error::InvalidConfig("engine CM ID lost its route context token".into()),
            actions,
        );
        return Ok(false);
    };
    let context_route = CmRouteToken::decode(context_token);
    if context_route != token {
        state.defer_cm_id(cm_id);
        connections.release_route(token, false);
        drop(reservation);
        request.complete_failure_into(
            Error::InvalidConfig("engine CM context token did not match its route".into()),
            actions,
        );
        return Ok(false);
    }
    let context_key = cm_id.context_key();
    assert!(connections.set_route_identity(token, cm_id.as_raw() as usize, context_key));
    let raw_id = cm_id.as_raw() as usize;
    if !state.insert_context_route(
        context_key,
        ContextRoute::Outbound {
            token: context_route,
            raw_id,
        },
    ) {
        state.defer_cm_id(cm_id);
        connections.release_route(token, false);
        drop(reservation);
        request.complete_failure_into(
            Error::InvalidConfig("duplicate CM context identity".into()),
            actions,
        );
        return Ok(false);
    }

    let resolve = cm_id.resolve_addr(None, &request.address, 2_000);
    match resolve {
        Ok(()) => {
            assert!(connections.set_outbound_state(
                token,
                OutboundState::AwaitAddr {
                    cm_id,
                    request,
                    reservation,
                },
            ));
        }
        Err(error) => {
            state.defer_cm_id(cm_id);
            state.retire_route(connections, token, false);
            drop(reservation);
            request.complete_failure_into(Error::from_v1(error), actions);
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn process_cancellation(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    request: Arc<OutboundRequest>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let encoded = request.route_token.load(Ordering::Acquire);
    if encoded == 0 {
        return Ok(());
    }
    let token = CmRouteToken::decode(encoded);
    let Lookup::Occupied(_) = connections.lookup_outbound(token) else {
        return Ok(());
    };
    let route_state = connections.take_outbound_state_if(token, |route_state| {
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
    let connection_token = connection.token;
    if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
        state.retire_route(connections, token, true);
        return Ok(());
    }
    connections.set_outbound_state(
        token,
        OutboundState::Closing {
            connection: connection.clone(),
        },
    );
    SessionReactorSources::begin_connection_close_into(
        shared,
        state,
        connections,
        connection_token,
        io_core,
        actions,
    );
    drop(request.take_result());
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
    drop(route_request);
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one CM event transaction keeps all reactor-owned state and evidence explicit"
)]
pub(super) fn handle_event(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: &EngineReactorResources,
    token: CmRouteToken,
    snapshot: CmEventSnapshot,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let disposition = if is_failure_event(snapshot.event_type) || snapshot.status != 0 {
        handle_failure_event(
            state,
            connections,
            shared,
            io_core,
            token,
            snapshot,
            actions,
        )?
    } else {
        match snapshot.event_type {
            CmEventType::AddrResolved => {
                handle_addr_resolved(state, connections, shared, resources, token, actions)
            }
            CmEventType::RouteResolved => handle_route_resolved(
                state,
                connections,
                shared,
                io_core,
                resources,
                token,
                actions,
            ),
            CmEventType::Established => {
                handle_established(state, connections, shared, io_core, token, actions)
            }
            CmEventType::Disconnected => {
                handle_disconnected(state, connections, shared, io_core, token, actions)
            }
            CmEventType::TimewaitExit => {
                if connections.outbound_is_disconnected(token) {
                    Ok(EventDisposition::Handled)
                } else {
                    Ok(EventDisposition::Rejected(CmEventReject::Unexpected))
                }
            }
            _ => Ok(EventDisposition::Rejected(CmEventReject::Unexpected)),
        }?
    };
    let route_retired = !matches!(connections.lookup_outbound(token), Lookup::Occupied(_));
    if is_failure_event(snapshot.event_type)
        || snapshot.status != 0
        || snapshot.event_type == CmEventType::RouteResolved
        || snapshot.event_type == CmEventType::Established
        || route_retired
        || !connections.outbound_is_establishing(token)
    {
        connections.finish_outbound_setup();
        shared.notify_reactor();
    }
    Ok(disposition)
}

fn handle_addr_resolved(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    resources: &EngineReactorResources,
    token: CmRouteToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let Some(OutboundState::AwaitAddr {
        cm_id,
        request,
        reservation,
    }) = connections.take_outbound_state_if(token, |route_state| {
        matches!(route_state, OutboundState::AwaitAddr { .. })
    })
    else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    if request.observer.completion.is_cancelled() || shared.shutdown_requested() {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(connections, token, true);
        request.complete_into(Err(Error::DriverShutdown), actions);
        return Ok(EventDisposition::Handled);
    }
    if let Err(error) = cm_id.require_context(resources.context.raw_context()) {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(connections, token, true);
        request.complete_failure_into(Error::from_v1(error), actions);
        return Ok(EventDisposition::Handled);
    }
    match cm_id.resolve_route(2_000) {
        Ok(()) => {
            connections.set_outbound_state(
                token,
                OutboundState::AwaitRoute {
                    cm_id,
                    request,
                    reservation,
                },
            );
        }
        Err(error) => {
            state.defer_cm_id(cm_id);
            drop(reservation);
            state.retire_route(connections, token, true);
            request.complete_failure_into(Error::from_v1(error), actions);
        }
    }
    Ok(EventDisposition::Handled)
}

fn handle_route_resolved(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: &EngineReactorResources,
    token: CmRouteToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let Some(OutboundState::AwaitRoute {
        cm_id,
        request,
        reservation,
    }) = connections.take_outbound_state_if(token, |route_state| {
        matches!(route_state, OutboundState::AwaitRoute { .. })
    })
    else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    if request.observer.completion.is_cancelled() || shared.shutdown_requested() {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(connections, token, true);
        request.complete_into(Err(Error::DriverShutdown), actions);
        return Ok(EventDisposition::Handled);
    }
    if let Err(error) = cm_id.require_context(resources.context.raw_context()) {
        state.defer_cm_id(cm_id);
        drop(reservation);
        state.retire_route(connections, token, true);
        request.complete_failure_into(Error::from_v1(error), actions);
        return Ok(EventDisposition::Handled);
    }

    let local_addr = cm_id.local_addr();
    let peer_addr = cm_id.peer_addr();
    let qp = match build_qp(resources, &cm_id, &request.config) {
        Ok(qp) => qp,
        Err(error) => {
            state.defer_cm_id(cm_id);
            drop(reservation);
            state.retire_route(connections, token, true);
            request.complete_failure_into(error, actions);
            return Ok(EventDisposition::Handled);
        }
    };
    let verbs = VerbsConnectionResources::new_shared(qp, cm_id);
    let connection = match install_reserved_connection(
        shared,
        connections,
        Some(token),
        verbs,
        request.config.clone(),
        local_addr,
        peer_addr,
        reservation,
    ) {
        Ok(connection) => connection,
        Err(failure) => {
            let (error, mut failed_resources) = failure.into_parts();
            match failed_resources.destroy_for_session(connections) {
                Ok((cm_id, _qp_destroyed)) => {
                    state.release_failed_install(connections, failed_resources)?;
                    if let Some(cm_id) = cm_id {
                        state.defer_cm_id(cm_id);
                    }
                    state.retire_route(connections, token, true);
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
                    connections
                        .set_outbound_state(token, OutboundState::Quarantined { connection });
                }
            }
            request.complete_failure_into(error, actions);
            return Ok(EventDisposition::Handled);
        }
    };
    let Lookup::Occupied(_) = connections.lookup(connection.session_token()) else {
        return Err(Error::InvalidConfig(
            "outbound connection entry disappeared during setup".into(),
        ));
    };

    let Some(setup) = request.take_setup() else {
        fail_registered_connection(
            state,
            connections,
            shared,
            io_core,
            token,
            request,
            connection,
            Error::InvalidConfig("outbound request setup was consumed more than once".into()),
            actions,
        )?;
        return Ok(EventDisposition::Handled);
    };
    let conn_param = match request.config.conn_param() {
        Ok(param) => param,
        Err(error) => {
            fail_registered_connection(
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
            return Ok(EventDisposition::Handled);
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
            if request.observer.completion.is_cancelled() || shared.shutdown_requested() {
                Err(Error::DriverShutdown)
            } else {
                Ok(())
            }
        },
        |connections| {
            connections
                .with_connection(connection.session_token(), |connection| {
                    connection.connect(&conn_param)
                })
                .ok_or(Error::TransportClosed)?
        },
    );
    if let Err(error) = establish {
        fail_registered_connection(
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
        return Ok(EventDisposition::Handled);
    }
    if request.observer.completion.is_cancelled() || shared.shutdown_requested() {
        fail_registered_connection(
            state,
            connections,
            shared,
            io_core,
            token,
            request,
            connection,
            Error::DriverShutdown,
            actions,
        )?;
        return Ok(EventDisposition::Handled);
    }
    connections.set_outbound_state(
        token,
        OutboundState::AwaitEstablished {
            request,
            connection,
        },
    );
    Ok(EventDisposition::Handled)
}

#[allow(
    clippy::too_many_arguments,
    reason = "CM failure transition dependencies stay explicit"
)]
fn fail_registered_connection(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    request: Arc<OutboundRequest>,
    connection: RdmaConnection,
    error: Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let connection_token = connection.session_token();
    let Lookup::Occupied(_) = connections.lookup(connection_token) else {
        return Err(Error::InvalidConfig(
            "registered outbound connection disappeared before close".into(),
        ));
    };
    connections.set_outbound_state(
        token,
        OutboundState::Closing {
            connection: EstablishedConnectionRoute::new(connection_token),
        },
    );
    SessionReactorSources::begin_connection_close_into(
        shared,
        state,
        connections,
        connection_token,
        io_core,
        actions,
    );
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
    if matches!(&error, Error::DriverShutdown) {
        request.complete_into(Err(error), actions);
    } else {
        request.complete_failure_into(error, actions);
    }
    Ok(())
}

fn handle_established(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let Some(OutboundState::AwaitEstablished {
        request,
        connection,
    }) = connections.take_outbound_state_if(token, |route_state| {
        matches!(route_state, OutboundState::AwaitEstablished { .. })
    })
    else {
        return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
    };
    if request.observer.completion.is_cancelled() || shared.shutdown_requested() {
        fail_registered_connection(
            state,
            connections,
            shared,
            io_core,
            token,
            request,
            connection,
            Error::DriverShutdown,
            actions,
        )?;
        return Ok(EventDisposition::Handled);
    }
    let waiter = Arc::clone(&request);
    let connection_token = connection.session_token();
    connections.set_outbound_state(
        token,
        OutboundState::EstablishedAwaitingDelivery {
            request,
            connection: EstablishedConnectionRoute::new(connection_token),
        },
    );
    if !connections.mark_active(connection_token) {
        return Err(Error::InvalidConfig(
            "outbound connection disappeared before establishment".into(),
        ));
    }
    waiter.complete_into(Ok(connection), actions);
    Ok(EventDisposition::Handled)
}

fn handle_disconnected(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let route_state = connections.take_outbound_state_if(token, |route_state| {
        matches!(
            route_state,
            OutboundState::AwaitEstablished { .. }
                | OutboundState::EstablishedAwaitingDelivery { .. }
        )
    });
    let Some(route_state) = route_state else {
        if connections.outbound_is_disconnected(token) {
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        return Ok(EventDisposition::Rejected(CmEventReject::Unexpected));
    };
    let (request, connection) = match route_state {
        OutboundState::AwaitEstablished {
            request,
            connection,
        } => {
            fail_registered_connection(
                state,
                connections,
                shared,
                io_core,
                token,
                request,
                connection,
                Error::Verbs(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "peer disconnected before outbound establishment",
                )),
                actions,
            )?;
            return Ok(EventDisposition::Handled);
        }
        OutboundState::EstablishedAwaitingDelivery {
            request,
            connection,
        } => (Some(request), connection),
        _ => unreachable!("disconnect state was pre-filtered"),
    };
    let connection_token = connection.token;
    if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
        state.retire_route(connections, token, true);
        return Ok(EventDisposition::Handled);
    }
    if let Some(event) = connections
        .with_connection_mut(connection_token, ConnectionState::mark_disconnected)
        .flatten()
    {
        actions.push_event(event);
    }
    let awaiting_delivery = request
        .as_ref()
        .is_some_and(|request| !request.observer.delivered.load(Ordering::Acquire));
    if awaiting_delivery {
        connections.set_outbound_state(
            token,
            OutboundState::DisconnectedAwaitingDelivery {
                request: request.expect("awaiting delivery retains its request"),
                connection: connection.clone(),
            },
        );
    } else {
        connections.set_outbound_state(
            token,
            OutboundState::Disconnected {
                connection: connection.clone(),
            },
        );
    }
    SessionReactorSources::begin_connection_close_into(
        shared,
        state,
        connections,
        connection_token,
        io_core,
        actions,
    );
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

fn handle_failure_event(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionContext,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: CmRouteToken,
    snapshot: CmEventSnapshot,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<EventDisposition> {
    let message = format!(
        "RDMA CM {:?} failed with status {} for id={:#x} listen_id={:#x}",
        snapshot.event_type, snapshot.status, snapshot.id, snapshot.listen_id
    );
    let route_state = connections.take_outbound_state_if(token, |_| true);
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
                request.observer.completion.is_cancelled() || shared.shutdown_requested();
            state.defer_cm_id(cm_id);
            drop(reservation);
            state.retire_route(connections, token, true);
            if shutdown_won {
                tracing::debug!(
                    cm_event = ?snapshot.event_type,
                    status = snapshot.status,
                    id = snapshot.id,
                    listen_id = snapshot.listen_id,
                    failure = %message,
                    "ignoring outbound CM setup failure after shutdown won the request"
                );
                request.complete_into(Err(Error::DriverShutdown), actions);
                return Ok(EventDisposition::IgnoredAfterShutdown);
            }
            request.complete_failure_into(Error::Verbs(std::io::Error::other(message)), actions);
        }
        OutboundState::AwaitEstablished {
            request,
            connection,
        } => {
            fail_registered_connection(
                state,
                connections,
                shared,
                io_core,
                token,
                request,
                connection,
                Error::Verbs(std::io::Error::other(message)),
                actions,
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
            let connection_token = connection.token;
            if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
                state.retire_route(connections, token, true);
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
            if request.observer.delivered.load(Ordering::Acquire) {
                connections.set_outbound_state(
                    token,
                    OutboundState::Failed {
                        connection: connection.clone(),
                    },
                );
            } else {
                connections.set_outbound_state(
                    token,
                    OutboundState::FailedAwaitingDelivery {
                        request,
                        connection: connection.clone(),
                    },
                );
            }
            SessionReactorSources::begin_connection_close_into(
                shared,
                state,
                connections,
                connection_token,
                io_core,
                actions,
            );
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
        OutboundState::Disconnected { connection } => {
            let connection_token = connection.token;
            if !matches!(connections.lookup(connection_token), Lookup::Occupied(_)) {
                state.retire_route(connections, token, true);
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
            connections.set_outbound_state(
                token,
                OutboundState::Failed {
                    connection: connection.clone(),
                },
            );
            SessionReactorSources::begin_connection_close_into(
                shared,
                state,
                connections,
                connection_token,
                io_core,
                actions,
            );
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
        OutboundState::FailedAwaitingDelivery {
            request,
            connection,
        } => {
            connections.set_outbound_state(
                token,
                OutboundState::FailedAwaitingDelivery {
                    request,
                    connection,
                },
            );
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        OutboundState::Failed { connection } => {
            connections.set_outbound_state(token, OutboundState::Failed { connection });
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        OutboundState::Closing { connection } => {
            connections.set_outbound_state(token, OutboundState::Closing { connection });
            return Ok(EventDisposition::Rejected(CmEventReject::Duplicate));
        }
        route_state @ OutboundState::Quarantined { .. } => {
            connections.set_outbound_state(token, route_state);
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

pub(in crate::v2::engine) struct OutboundRequest {
    pub(super) address: SocketAddr,
    pub(super) config: RdmaConnectionConfig,
    setup: Mutex<Option<ConnectionSetup>>,
    pub(super) observer: Arc<OutboundRequestObserver>,
    cancellation_enqueued: AtomicBool,
    pub(super) route_token: AtomicU64,
}

pub(super) struct OutboundRequestObserver {
    completion: CommandCompletion<RdmaConnection>,
    pub(super) delivered: AtomicBool,
}

impl OutboundRequest {
    pub(in crate::v2::engine) fn new(
        address: SocketAddr,
        config: RdmaConnectionConfig,
        setup: ConnectionSetup,
    ) -> Self {
        Self {
            address,
            config,
            setup: Mutex::new(Some(setup)),
            observer: Arc::new(OutboundRequestObserver {
                completion: CommandCompletion::new(),
                delivered: AtomicBool::new(false),
            }),
            cancellation_enqueued: AtomicBool::new(false),
            route_token: AtomicU64::new(0),
        }
    }

    pub(super) fn take_setup(&self) -> Option<ConnectionSetup> {
        lock_unpoison(&self.setup).take()
    }

    pub(super) fn complete_into(
        &self,
        result: Result<RdmaConnection>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.observer
            .completion
            .complete_into(result, false, actions);
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn complete_failure(&self, error: Error) {
        self.observer.completion.complete(Err(error));
    }

    pub(in crate::v2::engine) fn complete_failure_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.observer
            .completion
            .complete_into(Err(error), false, actions);
    }

    pub(in crate::v2::engine) fn try_enqueue_cancellation(&self) -> bool {
        !self.cancellation_enqueued.swap(true, Ordering::AcqRel)
    }

    #[cfg(test)]
    pub(super) fn cancel(&self, error: Error) {
        self.observer.cancel(error);
    }

    pub(super) fn cancel_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.observer.cancel_into(error, actions);
    }

    pub(super) fn take_result(&self) -> Option<Result<RdmaConnection>> {
        self.observer.take_result()
    }
}

impl OutboundRequestObserver {
    pub(super) fn take_result(&self) -> Option<Result<RdmaConnection>> {
        self.completion.take_result()
    }

    fn cancel(&self, error: Error) {
        drop(self.completion.cancel(error));
    }

    fn cancel_into(&self, error: Error, actions: &mut crate::v2::engine::reactor::ReactorActions) {
        drop(self.completion.cancel_into(error, false, actions));
    }
}

pub(super) struct ConnectWaiter {
    pub(super) commands: Weak<CommandIngress>,
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
        self.observer.completion.register(cx.waker());
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
    }
}

impl Drop for ConnectWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.observer.cancel(Error::DriverShutdown);
        let Some(request) = self.request.upgrade() else {
            return;
        };
        if let Some(commands) = self.commands.upgrade()
            && commands.cancel_connect(&request)
        {
            return;
        }
        if let Some(commands) = self.commands.upgrade() {
            commands.request_connect_cancel(request);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::v2::engine::CompletionMode;
    use crate::v2::engine::session::connection::TestConnectionProvider;
    use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
    use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

    struct ClosePoster {
        steps: Mutex<Vec<&'static str>>,
    }

    impl TestConnectionProvider for ClosePoster {
        fn qp_num(&self) -> u32 {
            81
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
            Ok(())
        }
    }

    #[test]
    fn disconnect_before_outbound_establishment_resolves_the_connect_request() {
        let (_engine, mut driver) = crate::v2::engine::test_engine_pair(CompletionMode::Polling);
        let poster = Arc::new(ClosePoster {
            steps: Mutex::new(Vec::new()),
        });
        let request = Arc::new(OutboundRequest::new(
            "127.0.0.1:7471".parse().unwrap(),
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        ));
        let reservation = driver
            .reactor
            .session
            .connections
            .try_reserve()
            .expect("test connection admission is available");
        let route = driver
            .reactor
            .session
            .connections
            .register_outbound(|route| OutboundRoute::new(route, Arc::clone(&request)))
            .unwrap();
        let connection = install_reserved_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            Some(route),
            Arc::clone(&poster),
            RdmaConnectionConfig::default(),
            None,
            None,
            reservation,
        )
        .unwrap();
        assert!(driver.reactor.session.connections.set_outbound_state(
            route,
            OutboundState::AwaitEstablished {
                request: Arc::clone(&request),
                connection,
            },
        ));

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        assert!(matches!(
            handle_disconnected(
                &mut driver.reactor.session.cm,
                &mut driver.reactor.session.connections,
                &driver.reactor.session.manager,
                driver.reactor.io.core_mut(),
                route,
                &mut actions,
            )
            .unwrap(),
            EventDisposition::Handled
        ));
        assert!(matches!(
            request.take_result(),
            Some(Err(Error::Verbs(error)))
                if error.kind() == std::io::ErrorKind::ConnectionAborted
        ));
        assert!(!matches!(
            driver.reactor.session.connections.lookup_outbound(route),
            Lookup::Occupied(_)
        ));
        assert_eq!(
            *poster
                .steps
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            vec!["to_error", "destroy_qp"]
        );
        actions.publish();
    }
}
