use std::sync::Arc;

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
    let cm = CmState::new(2).unwrap();
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

    let listener = ListenerState::test_only(1);
    let inbound = connections
        .register_inbound(|token| InboundRoute::new(token, Arc::downgrade(&listener)))
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
    let cm = CmState::new(1).unwrap();
    let listener = ListenerState::test_only(9);
    assert!(cm.insert_listener_identity(9, 33, Arc::clone(&listener)));
    assert!(cm.insert_context_route(
        44,
        ContextRoute::Listener {
            token: 9,
            raw_id: 33,
        },
    ));

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
        Ok(CmDispatchRoute::Listener(found)) if Arc::ptr_eq(&found, &listener)
    ));
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
