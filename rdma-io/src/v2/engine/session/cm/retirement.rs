//! QP-proven route retirement and event-drained CM identifier destruction.

#[cfg(test)]
use std::sync::atomic::Ordering;

use super::{
    CmState, ConnectionCmRoute, ConnectionToken, EstablishedConnectionRoute,
    FailedConnectionInstallResources, InboundRetirementCompletion, Lookup, PendingCmDestruction,
    RouteRetirement, RouteRetirementDisposition, SessionManager, contextual_cm_error, error_detail,
};
#[cfg(test)]
use super::{TestCmDestruction, injected_cm_result};
use crate::v2::engine::session::registry::ConnectionRegistry;
use crate::v2::error::{Error, Result};

pub(super) fn service_cm_destructions(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    io_core: &mut crate::v2::engine::io_core::IoState,
    resources: &super::EngineReactorResources,
    budget: usize,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<usize> {
    service_cm_destructions_with_probe(
        state,
        connections,
        io_core,
        budget,
        actions,
        |state, connections| state.defer_one_event(connections, resources),
    )
}

pub(super) fn service_cm_destructions_with_probe(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    io_core: &mut crate::v2::engine::io_core::IoState,
    budget: usize,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
    mut defer_one_event: impl FnMut(&mut CmState, &ConnectionRegistry) -> Result<bool>,
) -> Result<usize> {
    let mut processed = 0;
    while processed < budget {
        let pending = state.cm_destructions.pop_front();
        let Some(pending) = pending else {
            break;
        };
        match defer_one_event(state, connections) {
            Ok(true) => {
                state.cm_destructions.push_back(pending);
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
                    PendingCmDestruction::Route(cm_id) => {
                        if let Err((cm_id, error)) = cm_id.try_destroy() {
                            state
                                .cm_destructions
                                .push_front(PendingCmDestruction::Route(cm_id));
                            return Err(contextual_cm_error("destroy retired route CM ID", error));
                        }
                    }
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
                        let address = state.listeners.get(listener).map_or_else(
                            || "<stale listener>".into(),
                            |listener| listener.display_addr().to_string(),
                        );
                        match cm_id.try_destroy() {
                            Ok(()) => {
                                complete_listener_cm_destruction(state, listener, Ok(()), actions)?;
                            }
                            Err((cm_id, error)) => {
                                let error = contextual_cm_error(
                                    format!("destroy listener CM ID for {address}"),
                                    error,
                                );
                                if let Some(listener_entry) = state.listeners.get(listener) {
                                    listener_entry.finish_close_into(Some(error.clone()), actions);
                                }
                                state
                                    .cm_destructions
                                    .push_front(PendingCmDestruction::Listener { cm_id, listener });
                                return Err(error);
                            }
                        }
                    }
                    #[cfg(test)]
                    PendingCmDestruction::Test {
                        destroy_count,
                        target,
                    } => {
                        destroy_count.fetch_add(1, Ordering::AcqRel);
                        match target {
                            TestCmDestruction::Route => {}
                            TestCmDestruction::Listener {
                                listener,
                                destroy_error,
                            } => complete_listener_cm_destruction(
                                state,
                                listener,
                                injected_cm_result(destroy_error),
                                actions,
                            )?,
                        }
                    }
                }
            }
            Err(error) => {
                state.cm_destructions.push_front(pending);
                return Err(error);
            }
        }
        processed += 1;
    }
    Ok(processed)
}

fn complete_listener_cm_destruction(
    state: &mut CmState,
    listener: crate::v2::engine::registry::ListenerToken,
    result: Result<()>,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    match result {
        Ok(()) => {
            if let Some(listener) = state.listeners.get(listener) {
                listener.finish_close_into(None, actions);
            }
            state.listeners.release(listener, true);
            Ok(())
        }
        Err(error) => {
            if let Some(listener) = state.listeners.get(listener) {
                listener.finish_close_into(Some(error.clone()), actions);
            }
            Err(error)
        }
    }
}

fn complete_connection_cm_destruction(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    io_core: &mut crate::v2::engine::io_core::IoState,
    token: ConnectionToken,
    completion: Option<InboundRetirementCompletion>,
    cm_id: super::SharedCmId,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
) -> Result<()> {
    match cm_id.try_destroy() {
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
        Err((cm_id, error)) => {
            let error = contextual_cm_error(
                format!(
                    "destroy connection CM ID for slot {} generation {}",
                    token.slot, token.generation
                ),
                error,
            );
            retain_failed_connection_cm(
                state,
                connections,
                token,
                completion,
                cm_id,
                error,
                actions,
                true,
            )
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the complete failed provider bundle is transferred atomically"
)]
fn retain_failed_connection_cm(
    state: &mut CmState,
    connections: &mut ConnectionRegistry,
    token: ConnectionToken,
    completion: Option<InboundRetirementCompletion>,
    cm_id: super::SharedCmId,
    error: Error,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
    retry_at_front: bool,
) -> Result<()> {
    let newly_quarantined = connections.track_bundle_quarantine(token);
    if !newly_quarantined && !connections.is_quarantined(token) {
        return Err(Error::InvalidConfig(
            "connection CM failure lost its retiring ownership bundle".into(),
        ));
    }
    if newly_quarantined {
        let (_, event) = connections
            .with_connection_mut(token, |connection| {
                connection.publish_destroy_quarantine_into(&error, || {}, actions)
            })
            .unwrap_or((false, None));
        if let Some(event) = event {
            actions.push_event(event);
        }
    }
    let pending = PendingCmDestruction::Connection {
        cm_id,
        token,
        completion,
    };
    if retry_at_front {
        state.cm_destructions.push_front(pending);
    } else {
        state.cm_destructions.push_back(pending);
    }
    Err(error)
}

fn quarantine_connection_retirement(
    state: &mut CmState,
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
    _state: &mut CmState,
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
    state: &mut CmState,
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
        && let Some(listener) = completion.listener
        && state
            .listeners
            .get_mut(listener)
            .is_some_and(|listener| listener.finish_selected_route(completion.route))
    {
        state.enqueue_listener_work(listener);
    }
}

fn fail_inbound_retirement(
    state: &mut CmState,
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
        && let Some(listener) = completion.listener
        && state
            .listeners
            .get_mut(listener)
            .is_some_and(|listener| listener.finish_selected_route(completion.route))
    {
        state.enqueue_listener_work(listener);
    }
}

pub(super) fn retire_registered_connection_into(
    state: &mut CmState,
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
            disposition: RouteRetirementDisposition::None,
        },
    };
    let (completion, disposition) = match retirement {
        RouteRetirement::Complete {
            completion,
            disposition,
        } => (completion, disposition),
        RouteRetirement::Retry => {
            connections.retry_retirement(token);
            return Ok(());
        }
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
        if let RouteRetirementDisposition::Reject(reason) = disposition
            && let Err(error) = cm_id.reject(&[])
        {
            let error = contextual_cm_error(
                format!("reject selected inbound child after setup rollback ({reason:?})"),
                Error::from_v1(error),
            );
            return retain_failed_connection_cm(
                state,
                connections,
                token,
                completion,
                cm_id,
                error,
                actions,
                false,
            );
        }
        state
            .cm_destructions
            .push_back(PendingCmDestruction::Connection {
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

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use rdma_io_sys::rdmacm::rdma_cm_id;

    use super::*;
    use crate::v2::engine::{CompletionMode, test_engine_pair};

    #[test]
    fn failed_connection_cm_disposition_retains_exact_owner_and_completion() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let connection = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 1)
            .unwrap()
            .pop()
            .unwrap();
        let token = connection.session_token();
        let raw = NonNull::<rdma_cm_id>::dangling().as_ptr();
        let cm_id = super::super::SharedCmId::from_raw_for_ownership_test(raw);
        let completion = InboundRetirementCompletion {
            listener: None,
            route: 77,
            request: None,
            result: Some(Error::DriverShutdown),
            selected: true,
        };
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();

        assert!(
            retain_failed_connection_cm(
                &mut driver.reactor.session.cm,
                &mut driver.reactor.session.connections,
                token,
                Some(completion),
                cm_id,
                Error::InvalidConfig("injected CM disposition failure".into()),
                &mut actions,
                false,
            )
            .is_err()
        );
        assert!(driver.reactor.session.connections.is_quarantined(token));

        let pending = driver
            .reactor
            .session
            .cm
            .cm_destructions
            .pop_back()
            .expect("failed disposition retains one pending owner");
        let PendingCmDestruction::Connection {
            cm_id,
            token: retained_token,
            completion: Some(completion),
        } = pending
        else {
            panic!("failed disposition did not retain the complete connection CM bundle");
        };
        assert_eq!(retained_token, token);
        assert_eq!(cm_id.as_raw(), raw);
        assert_eq!(completion.route, 77);
        assert!(completion.selected);
        assert!(matches!(&completion.result, Some(Error::DriverShutdown)));

        assert!(
            retain_failed_connection_cm(
                &mut driver.reactor.session.cm,
                &mut driver.reactor.session.connections,
                token,
                Some(completion),
                cm_id,
                Error::InvalidConfig("repeated CM destruction failure".into()),
                &mut actions,
                true,
            )
            .is_err()
        );
        let pending = driver
            .reactor
            .session
            .cm
            .cm_destructions
            .pop_front()
            .expect("repeated failure requeues the complete owner");
        let PendingCmDestruction::Connection {
            mut cm_id,
            token: repeated_token,
            completion: Some(repeated_completion),
        } = pending
        else {
            panic!("repeated failure lost the complete connection CM bundle");
        };
        assert_eq!(repeated_token, token);
        assert_eq!(cm_id.as_raw(), raw);
        assert_eq!(repeated_completion.route, 77);
        assert!(repeated_completion.selected);
        cm_id.disarm_destroy_for_test();

        drop(connection);
        actions.publish();
    }
}
