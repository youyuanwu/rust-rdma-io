//! QP-proven route retirement and event-drained CM identifier destruction.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;

use super::{
    CmRouteToken, CmState, ConnectionCmRoute, ConnectionState, ConnectionToken,
    EstablishedConnectionRoute, FailedConnectionInstallResources, InboundRetirementCompletion,
    InboundState, Lookup, OutboundState, PendingCmDestruction, RetainedSetupRollback,
    RouteRetirement, SessionManager, connection_destruction_error, contextual_cm_error,
    error_detail, lock_unpoison,
};
#[cfg(test)]
use super::{TestCmDestruction, injected_cm_result};
use crate::v2::error::{Error, Result};

pub(super) fn service_cm_destructions(
    state: &CmState,
    shared: &SessionManager,
    budget: usize,
    mut try_process_event: impl FnMut() -> Result<bool>,
) -> Result<usize> {
    let mut processed = 0;
    while processed < budget {
        let pending = { lock_unpoison(&state.cm_destructions).pop_front() };
        let Some(pending) = pending else {
            break;
        };
        match try_process_event() {
            Ok(true) => {
                lock_unpoison(&state.cm_destructions).push_back(pending);
            }
            Ok(false) => {
                #[cfg(any(test, feature = "test-hooks"))]
                if let Some(cm_id) = pending.cm_id() {
                    crate::test_support::destruction::record(
                        crate::test_support::destruction::DestructionKind::CmDrainToWouldBlock,
                        cm_id.as_raw() as usize,
                    );
                }
                state.remove_owned_context_route(pending.cm_id());
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
                        let finalize_result = release_connection_retirement(shared, &connection);
                        complete_connection_cm_destruction(
                            state,
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
                        complete_listener_cm_destruction(listener, destroy_result)?;
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
                            } => complete_listener_cm_destruction(
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
                                    None => release_connection_retirement(shared, &connection),
                                };
                                complete_connection_cm_destruction(
                                    state,
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
                lock_unpoison(&state.cm_destructions).push_front(pending);
                return Err(error);
            }
        }
        processed += 1;
    }
    Ok(processed)
}

fn complete_listener_cm_destruction(
    listener: Arc<super::ListenerState>,
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
    state: &CmState,
    shared: &SessionManager,
    connection: Arc<ConnectionState>,
    completion: Option<InboundRetirementCompletion>,
    destroy_result: Result<()>,
    finalize_result: Result<()>,
) -> Result<()> {
    match (destroy_result, finalize_result) {
        (Ok(()), Ok(())) => {
            shared.record_connection_retired(&connection);
            if let Some(event) = connection.finish_retirement() {
                event.deliver();
            }
            finish_inbound_retirement(state, completion);
            Ok(())
        }
        (destroy_result, finalize_result) => {
            let error = connection_destruction_error(destroy_result, finalize_result);
            let message = error_detail(&error);
            if let Some(event) = connection.fail_retirement(error.clone()) {
                event.deliver();
            }
            fail_inbound_retirement(state, completion, message);
            Err(error)
        }
    }
}

pub(super) fn release_failed_install(
    shared: &SessionManager,
    resources: FailedConnectionInstallResources,
) -> Result<()> {
    match resources {
        FailedConnectionInstallResources::Unregistered { .. } => Ok(()),
        FailedConnectionInstallResources::Registered(connection) => {
            let released = shared
                .connections
                .release_unindexed(connection.token)
                .ok_or_else(|| {
                    Error::InvalidConfig(
                        "failed connection installation lost its reserved generation".into(),
                    )
                })?;
            if !Arc::ptr_eq(&released, &connection) {
                return Err(Error::InvalidConfig(
                    "failed connection installation released a mismatched generation".into(),
                ));
            }
            connection.release_admission();
            Ok(())
        }
    }
}

pub(super) fn retain_failed_install(
    state: &CmState,
    shared: &SessionManager,
    resources: FailedConnectionInstallResources,
    destroy_error: &Error,
) -> Option<EstablishedConnectionRoute> {
    match resources {
        FailedConnectionInstallResources::Unregistered {
            poster,
            mut reservation,
        } => {
            reservation.retain_setup_quarantine();
            lock_unpoison(&state.setup_rollback_quarantines).push(RetainedSetupRollback {
                _poster: poster,
                _reservation: reservation,
            });
            None
        }
        FailedConnectionInstallResources::Registered(connection) => {
            connection.begin_close();
            let _ = connection.try_begin_retirement();
            shared.track_connection_quarantine(connection.token);
            let (_, event) = connection.publish_destroy_quarantine(destroy_error, || {});
            if let Some(event) = event {
                event.deliver();
            }
            Some(EstablishedConnectionRoute::new(&connection))
        }
    }
}

pub(super) fn record_setup_rollback_quarantine(destroy_error: &Error) {
    tracing::warn!(
        %destroy_error,
        "setup rollback QP destroy failed; retaining connection resources"
    );
}

fn finalize_connection_retirement(
    shared: &SessionManager,
    connection: Arc<ConnectionState>,
) -> Result<()> {
    release_connection_retirement(shared, &connection)?;
    shared.record_connection_retired(&connection);
    if let Some(event) = connection.finish_retirement() {
        event.deliver();
    }
    Ok(())
}

fn release_connection_retirement(
    shared: &SessionManager,
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

fn finish_inbound_retirement(state: &CmState, completion: Option<InboundRetirementCompletion>) {
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
        state.enqueue_listener_work(&listener);
    }
}

fn fail_inbound_retirement(
    state: &CmState,
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
        state.enqueue_listener_work(&listener);
    }
}

fn retire_outbound_connection_route(
    state: &CmState,
    encoded: u64,
    connection: &Arc<ConnectionState>,
) -> Result<RouteRetirement> {
    let token = CmRouteToken::decode(encoded);
    let route = match state.routes.lookup_cloned(token) {
        Lookup::Occupied(route) => route,
        Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
            return Ok(RouteRetirement::Complete {
                completion: None,
                reject: None,
            });
        }
    };
    let route_state =
        route.take_state_if(|route_state| route_state.references_connection(connection.token));
    match route_state {
        Some(
            OutboundState::EstablishedAwaitingDelivery { .. }
            | OutboundState::Established { .. }
            | OutboundState::DisconnectedAwaitingDelivery { .. }
            | OutboundState::Disconnected { .. }
            | OutboundState::FailedAwaitingDelivery { .. }
            | OutboundState::Failed { .. }
            | OutboundState::Closing { .. },
        ) => {
            state.retire_route(&route, true);
            Ok(RouteRetirement::Complete {
                completion: None,
                reject: None,
            })
        }
        Some(route_state) => {
            route.set_state(route_state);
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
    state: &CmState,
    encoded: u64,
    connection: &Arc<ConnectionState>,
) -> Result<RouteRetirement> {
    let token = CmRouteToken::decode(encoded);
    let route = match state.inbound_routes.lookup_cloned(token) {
        Lookup::Occupied(route) => route,
        Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => {
            return Ok(RouteRetirement::Complete {
                completion: None,
                reject: None,
            });
        }
    };
    let route_state =
        route.take_state_if(|route_state| route_state.references_connection(connection.token));
    match route_state {
        Some(InboundState::EstablishedAwaitingDelivery { request, .. }) => {
            let delivered = request.fail_undelivered(Error::DriverShutdown);
            if delivered
                && let Some(listener) = route.listener.upgrade()
                && listener.finish_selected_route(encoded)
            {
                state.enqueue_listener_work(&listener);
            }
            state.inbound_routes.release(token, true);
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
            state.inbound_routes.release(token, true);
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
            state.inbound_routes.release(token, true);
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
        Some(route_state) => {
            route.set_state(route_state);
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

pub(super) fn retire_registered_connection(
    shared: &SessionManager,
    token: ConnectionToken,
) -> Result<()> {
    let state = &shared.cm;
    let Lookup::Occupied(connection) = shared.connections.lookup(token) else {
        return Ok(());
    };
    if connection.accepted_count() != 0 {
        return Ok(());
    }
    if !connection.error_transition_complete() {
        state.enqueue_retirement(token);
        return Ok(());
    }
    if !connection.try_begin_retirement() {
        return Ok(());
    }
    let lifecycle = connection.lock_lifecycle();
    let qp_boundary = shared.ensure_qp_destroyed(&connection, &lifecycle);
    drop(lifecycle);
    if let Err(error) = qp_boundary {
        tracing::warn!(
            slot = connection.token.slot,
            generation = connection.token.generation,
            qp_num = connection.qp_num(),
            %error,
            "connection QP destroy failed; retaining CM route and ownership bundle"
        );
        shared.track_connection_quarantine(connection.token);
        let (_, event) = connection.publish_destroy_quarantine(&error, || {});
        if let Some(event) = event {
            event.deliver();
        }
        return Ok(());
    }
    let retirement = match connection.cm_route() {
        Some(ConnectionCmRoute::Outbound(encoded)) => {
            retire_outbound_connection_route(state, encoded, &connection)?
        }
        Some(ConnectionCmRoute::Inbound(encoded)) => {
            retire_inbound_connection_route(state, encoded, &connection)?
        }
        None => RouteRetirement::Complete {
            completion: None,
            reject: None,
        },
    };
    let RouteRetirement::Complete { completion, reject } = retirement else {
        connection.retry_retirement();
        state.enqueue_retirement(token);
        return Ok(());
    };
    let lifecycle = connection.lock_lifecycle();
    let resources = shared.destroy_connection_resources(&connection, &lifecycle);
    drop(lifecycle);
    let cm_id = match resources {
        Ok(resources) => resources,
        Err(error) => {
            tracing::warn!(
                slot = connection.token.slot,
                generation = connection.token.generation,
                qp_num = connection.qp_num(),
                %error,
                "connection resource finalization failed; retaining terminal quarantine"
            );
            shared.track_connection_quarantine(connection.token);
            let (_, event) = connection.publish_destroy_quarantine(&error, || {});
            if let Some(event) = event {
                event.deliver();
            }
            finish_inbound_retirement(state, completion);
            return Ok(());
        }
    };
    if let Some(cm_id) = cm_id {
        if reject.is_some() {
            cm_id.reject(&[]).map_err(|error| {
                contextual_cm_error(
                    "reject selected inbound child after setup rollback",
                    Error::from_v1(error),
                )
            })?;
        }
        lock_unpoison(&state.cm_destructions).push_back(PendingCmDestruction::Connection {
            cm_id,
            connection,
            completion,
        });
        return Ok(());
    }
    finalize_connection_retirement(shared, connection)?;
    finish_inbound_retirement(state, completion);
    Ok(())
}
