use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::cm::CmEventType;
use crate::v2::engine::session::registry::{ConnectionPhase, ConnectionRegistry};

fn snapshot(context_key: usize, id: usize) -> CmEventSnapshot {
    CmEventSnapshot {
        event_type: CmEventType::AddrResolved,
        status: 0,
        id,
        listen_id: 0,
        context_key,
    }
}

#[test]
fn outbound_context_route_resolves_only_the_exact_generation_and_raw_id() {
    let mut cm = CmState::new(2).unwrap();
    let mut connections = ConnectionRegistry::new(2).unwrap();
    let request = Arc::new(OutboundRequest::new(
        "127.0.0.1:7471".parse().unwrap(),
        RdmaConnectionConfig::default(),
        empty_connection_setup(),
    ));
    let token = connections
        .register_outbound(|token| OutboundRoute::new(token, request))
        .unwrap();
    assert!(connections.set_route_identity(token, 11, 17));
    assert!(cm.insert_context_route(17, ContextRoute::Outbound { token, raw_id: 11 },));

    assert!(matches!(
        cm.lookup_event_route(&connections, snapshot(17, 11)),
        Ok(found) if found.token == token && found.raw_id == 11
    ));
    assert!(matches!(
        cm.lookup_event_route(&connections, snapshot(17, 12)),
        Err(CmEventReject::WrongId)
    ));

    assert!(connections.release_route(token, true));
    assert!(matches!(
        cm.lookup_event_route(&connections, snapshot(17, 11)),
        Err(CmEventReject::Duplicate | CmEventReject::Stale)
    ));
}

#[test]
fn inbound_and_outbound_routes_share_one_connection_generation_space() {
    let mut connections = ConnectionRegistry::new(1).unwrap();
    let request = Arc::new(OutboundRequest::new(
        "127.0.0.1:7471".parse().unwrap(),
        RdmaConnectionConfig::default(),
        empty_connection_setup(),
    ));
    let outbound = connections
        .register_outbound(|token| OutboundRoute::new(token, request))
        .unwrap();
    assert_eq!(connections.phase(outbound), Some(ConnectionPhase::Outbound));
    assert!(connections.release_route(outbound, true));

    let listener = ListenerToken {
        slot: 0,
        generation: 1,
    };
    let inbound = connections
        .register_inbound(|token| InboundRoute::new(token, listener))
        .unwrap();
    assert!(connections.set_route_identity(inbound, 23, 29));
    assert_eq!(inbound.slot, outbound.slot);
    assert_eq!(inbound.generation, outbound.generation + 1);
    assert_eq!(connections.phase(inbound), Some(ConnectionPhase::Inbound));
    assert!(matches!(
        connections.lookup_inbound(inbound),
        Lookup::Occupied(route)
            if route.raw_id == 23
                && route.context_key == 29
                && route.direction
                    == crate::v2::engine::session::registry::ConnectionRouteDirection::Inbound
    ));
    assert!(matches!(
        connections.lookup_outbound(inbound),
        Lookup::Unknown
    ));
}

#[test]
fn listener_identity_remains_in_the_single_context_index() {
    let mut cm = CmState::new(3).unwrap();
    let listener = cm
        .listeners
        .reserve(
            "127.0.0.1:1".parse().unwrap(),
            RdmaListenerConfig::default().backlog(9),
        )
        .unwrap();
    assert!(cm.activate_listener_identity_for_test(
        listener,
        "127.0.0.1:1".parse().unwrap(),
        33,
        44,
    ));
    let duplicate_raw = cm
        .listeners
        .reserve(
            "127.0.0.1:2".parse().unwrap(),
            RdmaListenerConfig::default(),
        )
        .unwrap();
    assert!(!cm.activate_listener_identity_for_test(
        duplicate_raw,
        "127.0.0.1:2".parse().unwrap(),
        33,
        45,
    ));
    assert!(!cm.context_routes.contains_key(&45));
    let duplicate_context = cm
        .listeners
        .reserve(
            "127.0.0.1:3".parse().unwrap(),
            RdmaListenerConfig::default(),
        )
        .unwrap();
    assert!(!cm.activate_listener_identity_for_test(
        duplicate_context,
        "127.0.0.1:3".parse().unwrap(),
        34,
        44,
    ));
    assert_eq!(cm.listeners.token_for_raw(34), None);

    let route = cm.lookup_dispatch_route(
        &ConnectionRegistry::new(1).unwrap(),
        CmEventSnapshot {
            event_type: CmEventType::Established,
            status: 0,
            id: 33,
            listen_id: 0,
            context_key: 44,
        },
    );
    assert!(matches!(
        route,
        Ok(CmDispatchRoute::Listener(found)) if found == listener
    ));

    assert!(matches!(
        cm.lookup_dispatch_route(
            &ConnectionRegistry::new(1).unwrap(),
            CmEventSnapshot {
                event_type: CmEventType::Established,
                status: 0,
                id: 34,
                listen_id: 0,
                context_key: 44,
            },
        ),
        Err(CmEventReject::WrongId)
    ));
    cm.listeners.release(listener, true);
    assert!(matches!(
        cm.lookup_dispatch_route(
            &ConnectionRegistry::new(1).unwrap(),
            CmEventSnapshot {
                event_type: CmEventType::ConnectRequest,
                status: 0,
                id: 77,
                listen_id: 33,
                context_key: 0,
            },
        ),
        Err(CmEventReject::Unknown)
    ));
    assert!(matches!(
        cm.lookup_dispatch_route(
            &ConnectionRegistry::new(1).unwrap(),
            CmEventSnapshot {
                event_type: CmEventType::Established,
                status: 0,
                id: 33,
                listen_id: 0,
                context_key: 44,
            },
        ),
        Err(CmEventReject::Duplicate | CmEventReject::Stale)
    ));
}

#[test]
fn listener_and_route_destructions_share_one_would_block_service() {
    let (_engine, mut driver) =
        crate::v2::engine::test_engine_pair(crate::v2::engine::CompletionMode::Polling);
    let (_listener, token) = driver
        .reactor
        .session
        .cm
        .test_listener(&driver.reactor.session.manager, 1);
    let route_destroys = Arc::new(AtomicUsize::new(0));
    let listener_destroys = Arc::new(AtomicUsize::new(0));
    driver
        .reactor
        .session
        .cm
        .defer_test_route_destruction(Arc::clone(&route_destroys));
    driver
        .reactor
        .session
        .cm
        .defer_test_listener_destruction(token, Arc::clone(&listener_destroys));

    let mut first_probe = true;
    let mut actions = crate::v2::engine::reactor::ReactorActions::default();
    assert_eq!(
        driver
            .reactor
            .session
            .cm
            .service_cm_destructions_with_probe(
                &mut driver.reactor.session.connections,
                driver.reactor.io.core_mut(),
                1,
                &mut actions,
                || {
                    let observed = first_probe;
                    first_probe = false;
                    Ok(observed)
                },
            )
            .unwrap(),
        1
    );
    assert_eq!(route_destroys.load(Ordering::Acquire), 0);
    assert_eq!(listener_destroys.load(Ordering::Acquire), 0);
    assert_eq!(driver.reactor.session.cm.destruction_work_count(), 2);

    assert_eq!(
        driver
            .reactor
            .session
            .cm
            .service_cm_destructions_with_probe(
                &mut driver.reactor.session.connections,
                driver.reactor.io.core_mut(),
                2,
                &mut actions,
                || Ok(false),
            )
            .unwrap(),
        2
    );
    actions.publish();
    assert_eq!(route_destroys.load(Ordering::Acquire), 1);
    assert_eq!(listener_destroys.load(Ordering::Acquire), 1);
    assert_eq!(driver.reactor.session.cm.listener_count(), 0);
}

#[test]
fn software_snapshot_reads_connection_work_from_the_reactor_registry() {
    let cm = CmState::new(1).unwrap();
    let mut connections = ConnectionRegistry::new(1).unwrap();
    let request = Arc::new(OutboundRequest::new(
        "127.0.0.1:7471".parse().unwrap(),
        RdmaConnectionConfig::default(),
        empty_connection_setup(),
    ));
    connections.enqueue_outbound(Arc::clone(&request), connections.try_reserve().unwrap());
    let snapshot = cm.software_snapshot(&connections);
    assert_eq!(snapshot.count(CmSoftwareClass::OutboundStart), 1);

    let (request, _reservation) = connections.pop_outbound().unwrap();
    connections.enqueue_cancellation(request);
    let snapshot = cm.software_snapshot(&connections);
    assert_eq!(snapshot.count(CmSoftwareClass::Cancellation), 1);
    assert_eq!(snapshot.count(CmSoftwareClass::OutboundStart), 0);
}
