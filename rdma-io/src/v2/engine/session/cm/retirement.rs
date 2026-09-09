//! QP-proven route retirement and event-drained CM identifier destruction.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;

use super::{
    CmState, ConnectionCmRoute, ConnectionToken, EstablishedConnectionRoute,
    FailedConnectionInstallResources, InboundRetirementCompletion, Lookup, PendingCmDestruction,
    RouteRetirement, SessionManager, contextual_cm_error, error_detail, lock_unpoison,
};
#[cfg(test)]
use super::{TestCmDestruction, injected_cm_result};
use crate::v2::engine::session::registry::ConnectionRegistry;
use crate::v2::error::{Error, Result};

pub(super) fn service_cm_destructions(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    io_core: &mut crate::v2::engine::io_core::IoState,
    budget: usize,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
    mut defer_one_event: impl FnMut(&ConnectionRegistry) -> Result<bool>,
) -> Result<usize> {
    let mut processed = 0;
    while processed < budget {
        let pending = { lock_unpoison(&state.cm_destructions).pop_front() };
        let Some(pending) = pending else {
            break;
        };
        match defer_one_event(connections) {
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
                        token,
                        completion,
                    } => {
                        complete_connection_cm_destruction(
                            state,
                            connections,
                            io_core,
                            token,
                            completion,
                            cm_id,
                            actions,
                        )?;
                    }
                    PendingCmDestruction::Listener { cm_id, listener } => {
                        let destroy_result = cm_id.destroy().map_err(|error| {
                            contextual_cm_error(
                                format!("destroy listener CM ID for {}", listener.local_addr),
                                error,
                            )
                        });
                        complete_listener_cm_destruction(listener, destroy_result, actions)?;
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
                                actions,
                            )?,
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
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    match result {
        Ok(()) => {
            listener.finish_close_into(None, actions);
            Ok(())
        }
        Err(error) => {
            listener.finish_close_into(Some(error.clone()), actions);
            Err(error)
        }
    }
}

fn complete_connection_cm_destruction(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: ConnectionToken,
    completion: Option<InboundRetirementCompletion>,
    cm_id: super::SharedCmId,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    match cm_id.destroy() {
        Ok(()) => match release_connection_retirement(connections, token) {
            Ok(mut connection) => {
                io_core.retire_connection_io(token, &connection.io_ledger);
                if let Some(event) = connection.finish_retirement_into(actions) {
                    actions.push_event(event);
                }
                finish_inbound_retirement(state, completion, actions);
                Ok(())
            }
            Err(error) => quarantine_connection_retirement(
                state,
                connections,
                token,
                completion,
                error,
                actions,
            ),
        },
        Err(error) => {
            let error = contextual_cm_error(
                format!(
                    "destroy connection CM ID for slot {} generation {}",
                    token.slot, token.generation
                ),
                error,
            );
            quarantine_connection_retirement(state, connections, token, completion, error, actions)
        }
    }
}

fn quarantine_connection_retirement(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    token: ConnectionToken,
    completion: Option<InboundRetirementCompletion>,
    error: Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let retained = connections.track_bundle_quarantine(token);
    if !retained {
        return Err(Error::InvalidConfig(
            "connection destruction failure lost its retiring ownership bundle".into(),
        ));
    }
    let (_, event) = connections
        .with_connection_mut(token, |connection| {
            connection.publish_destroy_quarantine_into(&error, || {}, actions)
        })
        .unwrap_or((false, None));
    if let Some(event) = event {
        actions.push_event(event);
    }
    tracing::warn!(
        slot = token.slot,
        generation = token.generation,
        %error,
        "connection destruction failed; retained the complete owning bundle"
    );
    fail_inbound_retirement(state, completion, error_detail(&error), actions);
    Ok(())
}

pub(super) fn release_failed_install(
    connections: &mut ConnectionRegistry,
    resources: FailedConnectionInstallResources,
) -> Result<()> {
    #[cfg(not(any(test, feature = "test-hooks")))]
    let _ = connections;
    match resources {
        FailedConnectionInstallResources::Unregistered { .. } => Ok(()),
        #[cfg(any(test, feature = "test-hooks"))]
        FailedConnectionInstallResources::Registered(token) => {
            let _released = connections.release_unindexed(token).ok_or_else(|| {
                Error::InvalidConfig(
                    "failed connection installation lost its reserved generation".into(),
                )
            })?;
            Ok(())
        }
        FailedConnectionInstallResources::Detached(_)
        | FailedConnectionInstallResources::Unrecoverable => Ok(()),
    }
}

pub(super) fn retain_failed_install(
    _state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    token: ConnectionToken,
    resources: FailedConnectionInstallResources,
    destroy_error: &Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Option<EstablishedConnectionRoute> {
    #[cfg(not(any(test, feature = "test-hooks")))]
    let _ = (shared, destroy_error, actions);
    match resources {
        FailedConnectionInstallResources::Unregistered {
            poster,
            reservation,
        } => {
            connections.retain_setup_quarantine(token, poster, reservation);
            None
        }
        #[cfg(any(test, feature = "test-hooks"))]
        FailedConnectionInstallResources::Registered(token) => {
            connections.begin_close(token);
            let _ = connections.request_retirement(token);
            let _ = connections.begin_retirement(token);
            shared.track_connection_quarantine(connections, token);
            let (_, event) = connections
                .with_connection_mut(token, |connection| {
                    connection.publish_destroy_quarantine_into(destroy_error, || {}, actions)
                })
                .unwrap_or((false, None));
            if let Some(event) = event {
                actions.push_event(event);
            }
            Some(EstablishedConnectionRoute::new(token))
        }
        FailedConnectionInstallResources::Detached(connection) => {
            connections.retain_detached_quarantine(token, connection);
            None
        }
        FailedConnectionInstallResources::Unrecoverable => None,
    }
}

pub(super) fn record_setup_rollback_quarantine(destroy_error: &Error) {
    tracing::warn!(
        %destroy_error,
        "setup rollback QP destroy failed; retaining connection resources"
    );
}

fn finalize_connection_retirement_into(
    connections: &mut ConnectionRegistry,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: ConnectionToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let mut connection = release_connection_retirement(connections, token)?;
    io_core.retire_connection_io(token, &connection.io_ledger);
    if let Some(event) = connection.finish_retirement_into(actions) {
        actions.push_event(event);
    }
    Ok(())
}

fn release_connection_retirement(
    connections: &mut ConnectionRegistry,
    token: ConnectionToken,
) -> Result<super::super::connection::ConnectionState> {
    let qp_num = connections
        .with_connection(token, |connection| connection.qp_num())
        .ok_or_else(|| {
            Error::InvalidConfig("connection registry retirement lost its entry".into())
        })?;
    let released = connections.release(token, qp_num).ok_or_else(|| {
        Error::InvalidConfig("connection registry retirement lost its entry".into())
    })?;
    Ok(released)
}

fn finish_inbound_retirement(
    state: &CmState,
    completion: Option<InboundRetirementCompletion>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) {
    let Some(completion) = completion else {
        return;
    };
    if let Some(request) = completion.request
        && let Some(result) = completion.result
    {
        request.complete_into(Err(result), actions);
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
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) {
    let Some(completion) = completion else {
        return;
    };
    if let Some(request) = completion.request {
        let _ =
            request.fail_undelivered_into(Error::Verbs(std::io::Error::other(message)), actions);
    }
    if completion.selected
        && let Some(listener) = completion.listener.upgrade()
        && listener.finish_selected_route(completion.route)
    {
        state.enqueue_listener_work(&listener);
    }
}

pub(super) fn retire_registered_connection_into(
    state: &CmState,
    connections: &mut ConnectionRegistry,
    shared: &SessionManager,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: ConnectionToken,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    let Lookup::Occupied(connection) = connections.lookup(token) else {
        return Ok(());
    };
    if connections.accepted_count(token) != 0 {
        return Ok(());
    }
    if !connections
        .with_connection(token, |connection| connection.error_transition_complete())
        .unwrap_or(false)
    {
        connections.enqueue_retirement(token);
        return Ok(());
    }
    if !connections.begin_retirement(token) {
        return Ok(());
    }
    let qp_boundary = shared.ensure_qp_destroyed(connections, token);
    if let Err(error) = qp_boundary {
        tracing::warn!(
            slot = token.slot,
            generation = token.generation,
            qp_num = connection.qp_num,
            %error,
            "connection QP destroy failed; retaining CM route and ownership bundle"
        );
        shared.track_connection_quarantine(connections, token);
        let (_, event) = connections
            .with_connection_mut(token, |connection| {
                connection.publish_destroy_quarantine_into(&error, || {}, actions)
            })
            .unwrap_or((false, None));
        if let Some(event) = event {
            actions.push_event(event);
        }
        return Ok(());
    }
    let retirement = match connections.connection_route(token) {
        Some(ConnectionCmRoute::Outbound(encoded)) => {
            state.retire_outbound_route_for_retirement(connections, encoded, token)?
        }
        Some(ConnectionCmRoute::Inbound(encoded)) => {
            state.retire_inbound_route_for_retirement(connections, encoded, token, actions)?
        }
        None => RouteRetirement::Complete {
            completion: None,
            reject: None,
        },
    };
    let RouteRetirement::Complete { completion, reject } = retirement else {
        connections.retry_retirement(token);
        return Ok(());
    };
    let outstanding_operations = connections.accepted_count(token);
    let resources = shared.destroy_connection_resources(connections, token, outstanding_operations);
    let cm_id = match resources {
        Ok(resources) => resources,
        Err(error) => {
            tracing::warn!(
                slot = token.slot,
                generation = token.generation,
                qp_num = connection.qp_num,
                %error,
                "connection resource finalization failed; retaining terminal quarantine"
            );
            shared.track_connection_quarantine(connections, token);
            let (_, event) = connections
                .with_connection_mut(token, |connection| {
                    connection.publish_destroy_quarantine_into(&error, || {}, actions)
                })
                .unwrap_or((false, None));
            if let Some(event) = event {
                actions.push_event(event);
            }
            finish_inbound_retirement(state, completion, actions);
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
            token,
            completion,
        });
        return Ok(());
    }
    finalize_connection_retirement_into(connections, io_core, token, actions)?;
    finish_inbound_retirement(state, completion, actions);
    Ok(())
}
