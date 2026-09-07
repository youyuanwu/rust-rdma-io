#![cfg(test)]

use super::*;
use crate::v2::engine::session::connection::{
    ConnectionAdmissionPool, WorkRequestPoster, install_connection,
};
use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};
use std::sync::Barrier;
use std::task::{Context, Poll};

#[test]
fn connect_waiter_observer_does_not_retain_outbound_record() {
    let request = Arc::new(test_request());
    let record = Arc::downgrade(&request);
    let observer = Arc::clone(&request.observer);
    request.complete(Err(Error::DriverShutdown));
    drop(request);

    assert!(record.upgrade().is_none());
    assert!(matches!(
        observer.take_result(),
        Some(Err(Error::DriverShutdown))
    ));
}

#[test]
fn pending_connect_future_releases_engine_and_manager_record_owners() {
    let (engine, _driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let baseline_engine_owners = Arc::strong_count(&engine.shared);
    let baseline_manager_owners = Arc::strong_count(&engine.shared.session);
    let mut connect = Box::pin(connect_with_setup(
        Arc::clone(&engine.shared.session),
        "127.0.0.1:7471".parse().unwrap(),
        RdmaConnectionConfig::default(),
        empty_connection_setup(),
    ));
    assert_eq!(
        Arc::strong_count(&engine.shared),
        baseline_engine_owners,
        "connect setup no longer retains the concrete engine root"
    );
    assert_eq!(
        Arc::strong_count(&engine.shared.session),
        baseline_manager_owners + 1
    );

    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(connect.as_mut().poll(&mut context).is_pending());
    assert_eq!(
        Arc::strong_count(&engine.shared),
        baseline_engine_owners,
        "suspended connect future must retain only a weak SessionManager route"
    );
    assert_eq!(
        Arc::strong_count(&engine.shared.session),
        baseline_manager_owners
    );
    let pending = lock_unpoison(&engine.shared.session.cm.pending);
    assert_eq!(pending.len(), 1);
    assert_eq!(
        Arc::strong_count(&pending[0]),
        1,
        "only SessionManager CM state may strongly retain the outbound record"
    );
}

#[test]
fn route_tokens_round_trip_without_wrapping_or_pointer_encoding() {
    let token = CmRouteToken {
        slot: 0x00ff_ee11,
        generation: 0xaabb_ccdd,
    };
    assert_eq!(CmRouteToken::decode(token.encode()), token);
}

#[test]
fn only_terminal_cm_classes_are_classified_as_failures() {
    for event in [
        CmEventType::AddrError,
        CmEventType::RouteError,
        CmEventType::ConnectError,
        CmEventType::Unreachable,
        CmEventType::Rejected,
        CmEventType::DeviceRemoval,
        CmEventType::AddrChange,
    ] {
        assert!(is_failure_event(event));
    }
    for event in [
        CmEventType::AddrResolved,
        CmEventType::RouteResolved,
        CmEventType::Established,
        CmEventType::Disconnected,
        CmEventType::TimewaitExit,
    ] {
        assert!(!is_failure_event(event));
    }
}

#[test]
fn former_pending_cancellation_lock_order_forms_an_abba_cycle() {
    let cm = Arc::new(CmState::new(1).unwrap());
    let first_locks_held = Arc::new(Barrier::new(2));
    let second_locks_checked = Arc::new(Barrier::new(2));

    let diagnostics_cm = Arc::clone(&cm);
    let diagnostics_first = Arc::clone(&first_locks_held);
    let diagnostics_checked = Arc::clone(&second_locks_checked);
    let diagnostics = std::thread::spawn(move || {
        let pending = lock_unpoison(&diagnostics_cm.pending);
        diagnostics_first.wait();
        assert!(
            diagnostics_cm.cancellations.try_lock().is_err(),
            "the former diagnostics order must observe cancellations held"
        );
        diagnostics_checked.wait();
        drop(pending);
    });

    let service_cm = Arc::clone(&cm);
    let service_first = Arc::clone(&first_locks_held);
    let service_checked = Arc::clone(&second_locks_checked);
    let service = std::thread::spawn(move || {
        let cancellations = lock_unpoison(&service_cm.cancellations);
        service_first.wait();
        assert!(
            service_cm.pending.try_lock().is_err(),
            "the former service order must observe pending held"
        );
        service_checked.wait();
        drop(cancellations);
    });

    diagnostics.join().unwrap();
    service.join().unwrap();
}

#[test]
fn route_queries_and_cm_service_complete_under_lock_order_stress() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let shared = Arc::clone(&engine.shared.session);
    let start = Arc::new(Barrier::new(3));

    let diagnostics_shared = Arc::clone(&shared);
    let diagnostics_start = Arc::clone(&start);
    let diagnostics = std::thread::spawn(move || {
        diagnostics_start.wait();
        for _ in 0..1_000 {
            let _ = diagnostics_shared.cm.pending_route_count();
            let _ = diagnostics_shared.cm.retained_owner_count();
            let _ = diagnostics_shared.cm.has_software_work();
        }
    });

    let service_shared = Arc::clone(&shared);
    let service_start = Arc::clone(&start);
    let service = std::thread::spawn(move || {
        service_start.wait();
        for _ in 0..1_000 {
            service_shared
                .cm
                .enqueue_cancellation(Arc::new(test_request()));
            assert_eq!(
                service_shared
                    .cm
                    .service_software(&service_shared, None, 1)
                    .unwrap(),
                1
            );
        }
    });

    start.wait();
    diagnostics.join().unwrap();
    service.join().unwrap();
    assert!(lock_unpoison(&shared.cm.cancellations).is_empty());

    drop(engine);
    drop(driver);
}

#[test]
fn retained_setup_rollback_route_is_counted_once() {
    let cm = CmState::new(1).unwrap();
    let request = Arc::new(test_request());
    cm.routes
        .allocate_with(|token| Arc::new(OutboundRoute::new(token, request)))
        .unwrap();
    let pool = ConnectionAdmissionPool::new(1);
    let mut reservation = pool.try_acquire().unwrap();
    assert!(reservation.retain_setup_quarantine());
    lock_unpoison(&cm.setup_rollback_quarantines).push(RetainedSetupRollback {
        _poster: Arc::new(NoopPoster(7)),
        _reservation: reservation,
    });

    assert_eq!(cm.retained_owner_count(), 1);
}

#[test]
fn route_registry_rejects_wrong_id_stale_duplicate_and_unknown_events() {
    let cm = CmState::new(2).unwrap();
    let request = Arc::new(test_request());
    let (token, route) = cm
        .routes
        .allocate_with(|token| Arc::new(OutboundRoute::new(token, request)))
        .unwrap();
    route.set_identity(0x1000, 0x2000);
    lock_unpoison(&cm.context_routes).insert(
        0x2000,
        ContextRoute::Outbound {
            token,
            raw_id: 0x1000,
        },
    );

    let exact = CmEventSnapshot {
        event_type: CmEventType::AddrResolved,
        status: 0,
        id: 0x1000,
        listen_id: 0,
        context_key: 0x2000,
    };
    assert!(cm.lookup_event_route(exact).is_ok());
    assert!(matches!(
        cm.lookup_event_route(CmEventSnapshot {
            id: 0x1001,
            ..exact
        }),
        Err(CmEventReject::WrongId)
    ));
    assert!(matches!(
        cm.lookup_event_route(CmEventSnapshot {
            context_key: 0x3000,
            ..exact
        }),
        Err(CmEventReject::Unknown)
    ));
    lock_unpoison(&cm.context_routes).insert(
        0x3001,
        ContextRoute::Outbound {
            token: CmRouteToken {
                slot: token.slot,
                generation: token.generation + 1,
            },
            raw_id: 0x1000,
        },
    );
    assert!(matches!(
        cm.lookup_event_route(CmEventSnapshot {
            context_key: 0x3001,
            ..exact
        }),
        Err(CmEventReject::Stale)
    ));

    cm.retire_route(&route, true);
    for event_type in [
        CmEventType::AddrResolved,
        CmEventType::Disconnected,
        CmEventType::TimewaitExit,
    ] {
        assert!(matches!(
            cm.lookup_event_route(CmEventSnapshot {
                event_type,
                ..exact
            }),
            Err(CmEventReject::Duplicate)
        ));
    }
}

#[test]
fn inbound_routes_use_exact_generational_context_and_id_identity() {
    let cm = CmState::new(2).unwrap();
    let listener = ListenerState::test_only(2);
    let (token, route) = cm
        .inbound_routes
        .allocate_with(|token| Arc::new(InboundRoute::new(token, Arc::downgrade(&listener))))
        .unwrap();
    route.set_identity(0x4000, 0x5000);
    lock_unpoison(&cm.context_routes).insert(
        0x5000,
        ContextRoute::Inbound {
            token,
            raw_id: 0x4000,
        },
    );
    let exact = CmEventSnapshot {
        event_type: CmEventType::Established,
        status: 0,
        id: 0x4000,
        listen_id: 0,
        context_key: 0x5000,
    };
    assert!(matches!(
        cm.lookup_dispatch_route(exact),
        Ok(CmDispatchRoute::Inbound(_))
    ));
    assert!(matches!(
        cm.lookup_dispatch_route(CmEventSnapshot {
            id: 0x4001,
            ..exact
        }),
        Err(CmEventReject::WrongId)
    ));
    cm.inbound_routes.release(token, true);
    assert!(matches!(
        cm.lookup_dispatch_route(exact),
        Err(CmEventReject::Duplicate)
    ));
    assert!(!cm.remove_context_route_if_owned(0x5000, 0x4001, Some(token.encode())));
    assert!(
        !cm.remove_context_route_if_owned(
            0x5000,
            0x4000,
            Some(
                CmRouteToken {
                    slot: token.slot,
                    generation: token.generation + 1,
                }
                .encode()
            )
        )
    );
    assert!(cm.remove_context_route_if_owned(0x5000, 0x4000, Some(token.encode())));
}

#[test]
fn unowned_child_context_never_removes_listener_event_route() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let listener = ListenerState::test_only(2);
    let listener_token = 17;
    lock_unpoison(&engine.shared.session.cm.listeners)
        .insert(listener_token, Arc::clone(&listener));
    lock_unpoison(&engine.shared.session.cm.context_routes).insert(
        0x5000,
        ContextRoute::Listener {
            token: listener_token,
            raw_id: 0x4000,
        },
    );

    assert!(
        !engine
            .shared
            .session
            .cm
            .remove_context_route_if_owned(0x5000, 0x4001, None)
    );
    assert!(matches!(
        lock_unpoison(&engine.shared.session.cm.context_routes)
            .get(&0x5000)
            .copied(),
        Some(ContextRoute::Listener {
            token: 17,
            raw_id: 0x4000
        })
    ));

    let snapshot = CmEventSnapshot {
        event_type: CmEventType::AddrChange,
        status: libc::EADDRNOTAVAIL,
        id: 0x4000,
        listen_id: 0,
        context_key: 0x5000,
    };
    let routed = engine
        .shared
        .session
        .cm
        .lookup_dispatch_route(snapshot)
        .unwrap();
    let CmDispatchRoute::Listener(routed_listener) = routed else {
        panic!("listener context route changed after unowned child rejection");
    };
    assert!(Arc::ptr_eq(&routed_listener, &listener));
    assert!(matches!(
        engine
            .shared
            .session
            .cm
            .handle_listener_event(&engine.shared.session, &listener, snapshot),
        Ok(EventDisposition::Handled)
    ));
    assert!(listener.is_closing());
    assert!(matches!(listener.close_error(), Error::Verbs(_)));

    let removed = CmEventSnapshot {
        event_type: CmEventType::DeviceRemoval,
        ..snapshot
    };
    assert!(matches!(
        engine
            .shared
            .session
            .cm
            .handle_listener_event(&engine.shared.session, &listener, removed),
        Err(Error::Verbs(_))
    ));

    let wrong_generation = CmRouteToken {
        slot: 0,
        generation: 2,
    }
    .encode();
    assert!(!engine.shared.session.cm.remove_context_route_if_owned(
        0x5000,
        0x4000,
        Some(wrong_generation)
    ));
    assert!(engine.shared.session.cm.remove_context_route_if_owned(
        0x5000,
        0x4000,
        Some(listener_token)
    ));
    drop(driver);
}

#[test]
fn duplicate_context_route_keeps_the_incumbent_mapping() {
    let cm = CmState::new(2).unwrap();
    let incumbent = CmRouteToken {
        slot: 0,
        generation: 1,
    };
    let duplicate = CmRouteToken {
        slot: 1,
        generation: 1,
    };

    assert!(cm.insert_context_route(
        0x2000,
        ContextRoute::Outbound {
            token: incumbent,
            raw_id: 0x1000,
        }
    ));
    assert!(!cm.insert_context_route(
        0x2000,
        ContextRoute::Outbound {
            token: duplicate,
            raw_id: 0x1001,
        }
    ));
    assert_eq!(
        lock_unpoison(&cm.context_routes).get(&0x2000).copied(),
        Some(ContextRoute::Outbound {
            token: incumbent,
            raw_id: 0x1000,
        })
    );
}

#[test]
fn duplicate_listener_identity_keeps_both_incumbent_mappings() {
    let cm = CmState::new(2).unwrap();
    let incumbent = ListenerState::test_only(1);
    let duplicate_token = ListenerState::test_only(1);
    let duplicate_id = ListenerState::test_only(1);

    assert!(cm.insert_listener_identity(7, 0x1000, Arc::clone(&incumbent)));
    assert!(!cm.insert_listener_identity(7, 0x2000, duplicate_token));
    assert!(!cm.insert_listener_identity(8, 0x1000, duplicate_id));
    assert!(
        lock_unpoison(&cm.listeners)
            .get(&7)
            .is_some_and(|listener| Arc::ptr_eq(listener, &incumbent))
    );
    assert_eq!(lock_unpoison(&cm.listeners).len(), 1);
    assert_eq!(
        lock_unpoison(&cm.listener_ids).get(&0x1000).copied(),
        Some(7)
    );
    assert_eq!(lock_unpoison(&cm.listener_ids).len(), 1);
}

#[test]
fn pre_establish_setup_completes_before_connect_and_failure_skips_connect() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let connection = install_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(7)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    )
    .unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let summary = run_setup_before_establish(
        recording_setup(Arc::clone(&order), Ok(SetupSummary { posted_wrs: 0 })),
        &connection,
        || {
            lock_unpoison(&order).push("pre-connect");
            Ok(())
        },
        || {
            lock_unpoison(&order).push("connect");
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(summary.posted_wrs, 0);
    assert_eq!(
        &*lock_unpoison(&order),
        &["setup", "pre-connect", "connect"]
    );

    let failed_connection = install_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(8)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    )
    .unwrap();
    lock_unpoison(&order).clear();
    let error = run_setup_before_establish(
        recording_setup(
            Arc::clone(&order),
            Err(Error::InvalidConfig("setup failed".into())),
        ),
        &failed_connection,
        || {
            lock_unpoison(&order).push("pre-connect");
            Ok(())
        },
        || {
            lock_unpoison(&order).push("connect");
            Ok(())
        },
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidConfig(_)));
    assert_eq!(&*lock_unpoison(&order), &["setup"]);

    let mismatched_connection = install_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(9)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    )
    .unwrap();
    lock_unpoison(&order).clear();
    let error = run_setup_before_establish(
        recording_setup(Arc::clone(&order), Ok(SetupSummary { posted_wrs: 1 })),
        &mismatched_connection,
        || {
            lock_unpoison(&order).push("pre-connect");
            Ok(())
        },
        || {
            lock_unpoison(&order).push("connect");
            Ok(())
        },
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidConfig(_)));
    assert_eq!(&*lock_unpoison(&order), &["setup"]);
    drop(mismatched_connection);
    drop(failed_connection);
    drop(connection);
    drop(driver);
}

#[test]
fn delivery_replaces_the_frontend_with_weak_generational_route_state() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let connection = install_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(11)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    )
    .unwrap();
    let request = Arc::new(test_request());
    let (_, route) = engine
        .shared
        .session
        .cm
        .routes
        .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
        .unwrap();
    route.set_state(OutboundState::EstablishedAwaitingDelivery {
        request: Arc::clone(&request),
        connection: EstablishedConnectionRoute::new(&connection.state),
    });
    request.complete(Ok(connection));
    let mut waiter = Box::pin(ConnectWaiter {
        manager: Arc::downgrade(&engine.shared.session),
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    });
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    let Poll::Ready(Ok(connection)) = waiter.as_mut().poll(&mut context) else {
        panic!("completed connection was not delivered");
    };
    drop(waiter);

    assert!(route.request().is_none());
    let state = lock_unpoison(&route.state);
    let OutboundState::Established { connection: routed } = &*state else {
        panic!("delivered route retained a pending-delivery state");
    };
    assert_eq!(routed.token, connection.state.token);
    assert!(routed.upgrade().is_some());
    drop(state);
    assert_eq!(
        Arc::strong_count(&engine.shared),
        2,
        "neither the route nor the test connection frontend retains the engine root"
    );

    engine.shared.session.cm.retire_route(&route, true);
    drop(connection);
    drop(engine);
    drop(driver);
}

#[test]
fn shutdown_replaces_an_undelivered_success_and_enqueues_route_cleanup() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let connection = install_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(12)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    )
    .unwrap();
    let request = Arc::new(test_request());
    let (_, route) = engine
        .shared
        .session
        .cm
        .routes
        .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
        .unwrap();
    route.set_state(OutboundState::EstablishedAwaitingDelivery {
        request: Arc::clone(&request),
        connection: EstablishedConnectionRoute::new(&connection.state),
    });
    request.complete(Ok(connection));

    engine.shared.session.cm.begin_shutdown(
        &engine.shared.session,
        &MemoizedTerminalResult::from_error(Error::DriverShutdown),
    );

    assert!(matches!(
        request.take_result(),
        Some(Err(Error::DriverShutdown))
    ));
    assert_eq!(
        lock_unpoison(&engine.shared.session.cm.cancellations).len(),
        1
    );

    let processed = engine
        .shared
        .session
        .cm
        .service_software(&engine.shared.session, None, 1)
        .unwrap();
    assert_eq!(processed, 1);
    assert!(lock_unpoison(&engine.shared.session.cm.cancellations).is_empty());
    let _ = engine
        .shared
        .session
        .cm
        .service_software(&engine.shared.session, None, 1)
        .unwrap();
    drop(engine);
    drop(driver);
}

#[test]
fn transitioning_route_requeues_retirement_once_per_service_pass() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let request = Arc::new(test_request());
    let (route_token, route) = engine
        .shared
        .session
        .cm
        .routes
        .allocate_with(|token| Arc::new(OutboundRoute::new(token, Arc::clone(&request))))
        .unwrap();
    let (admission, reservation) = reserve_connection(&engine.shared.session).unwrap();
    let connection = install_reserved_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(13)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
        reservation,
        Some(ConnectionCmRoute::Outbound(route_token.encode())),
    )
    .unwrap();
    drop(admission);
    // This scheduler-only fixture has no real QP, so record its synthetic
    // destruction boundary before exercising route-state retry behavior.
    let _proof = engine
        .shared
        .session
        .mint_qp_destruction_proof_for_test(&connection.state);

    engine
        .shared
        .session
        .cm
        .enqueue_retirement(connection.state.token);
    let processed = engine
        .shared
        .session
        .cm
        .service_software(&engine.shared.session, None, 32)
        .unwrap();
    assert_eq!(
        processed, 1,
        "a requeued retirement may run only once per service pass"
    );
    assert_eq!(
        lock_unpoison(&engine.shared.session.cm.retirements).len(),
        1
    );
    assert!(!connection.state.is_retired());

    route.set_state(OutboundState::Closing {
        connection: EstablishedConnectionRoute::new(&connection.state),
    });
    assert!(
        engine
            .shared
            .session
            .transition_connection_to_error(&connection.state)
            .unwrap()
    );
    let processed = engine
        .shared
        .session
        .cm
        .service_software(&engine.shared.session, None, 32)
        .unwrap();
    assert_eq!(processed, 1);
    assert!(lock_unpoison(&engine.shared.session.cm.retirements).is_empty());
    assert!(connection.state.is_retired());
    assert!(matches!(
        engine
            .shared
            .session
            .connections
            .lookup(connection.state.token),
        Lookup::Duplicate
    ));

    drop(connection);
    drop(request);
    drop(engine);
    drop(driver);
}

#[test]
fn inbound_disconnect_without_connection_state_fails_and_retires_selected_accept() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let listener = ListenerState::test_only(1);
    let (route_token, route) = engine
        .shared
        .session
        .cm
        .inbound_routes
        .allocate_with(|token| Arc::new(InboundRoute::new(token, Arc::downgrade(&listener))))
        .unwrap();
    let request = selected_accept(&listener, route_token.encode());
    let connection = Arc::new(ConnectionState::new(
        ConnectionToken {
            slot: 9,
            generation: 3,
        },
        Arc::new(NoopPoster(31)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
        None,
        None,
    ));
    route.set_state(InboundState::EstablishedAwaitingDelivery {
        request: Arc::clone(&request),
        connection: EstablishedConnectionRoute::new(&connection),
    });
    drop(connection);

    assert!(matches!(
        engine
            .shared
            .session
            .cm
            .handle_inbound_disconnected(&engine.shared.session, &route),
        Ok(EventDisposition::Handled)
    ));
    let Some(Err(error)) = request.take_result_for_test() else {
        panic!("selected accept must fail");
    };
    assert!(
        error
            .to_string()
            .contains("lost connection state before accept retirement")
    );
    assert!(matches!(
        engine
            .shared
            .session
            .cm
            .inbound_routes
            .lookup_cloned(route_token),
        Lookup::Duplicate
    ));

    drop(engine);
    drop(driver);
}

#[test]
fn listener_destroy_error_completes_close_once_before_propagation() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let listener_state = ListenerState::test_only(1);
    let listener = RdmaListener::from_state(&engine.shared.session, Arc::clone(&listener_state));
    let mut close = Box::pin(listener.close());
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(close.as_mut().poll(&mut cx).is_pending());
    let destroy_count = Arc::new(AtomicUsize::new(0));
    lock_unpoison(&engine.shared.session.cm.cm_destructions).push_back(
        PendingCmDestruction::Test {
            destroy_count: Arc::clone(&destroy_count),
            target: TestCmDestruction::Listener {
                listener: listener_state,
                destroy_error: Some(
                    "destroy listener CM ID for 127.0.0.1:1: injected failure".into(),
                ),
            },
        },
    );

    let error = engine
        .shared
        .session
        .cm
        .service_cm_destructions(&engine.shared.session, 1, || Ok(false))
        .unwrap_err();
    assert!(error.to_string().contains("injected failure"));
    let Poll::Ready(Err(close_error)) = close.as_mut().poll(&mut cx) else {
        panic!("listener close was not completed before destroy failure propagation");
    };
    assert_eq!(close_error.to_string(), error.to_string());
    assert_eq!(destroy_count.load(Ordering::Acquire), 1);
    assert!(lock_unpoison(&engine.shared.session.cm.cm_destructions).is_empty());
    assert_eq!(
        engine
            .shared
            .session
            .cm
            .service_cm_destructions(&engine.shared.session, 1, || Ok(false))
            .unwrap(),
        0
    );
    assert_eq!(destroy_count.load(Ordering::Acquire), 1);

    drop(close);
    drop(listener);
    drop(engine);
    drop(driver);
}

#[test]
fn connection_destroy_error_fails_accept_and_retirement_once() {
    assert_connection_cm_destruction_failure(
        Some("destroy connection CM ID: injected destroy failure"),
        None,
        "injected destroy failure",
        false,
    );
}

#[test]
fn connection_finalize_error_fails_accept_and_retirement_once() {
    assert_connection_cm_destruction_failure(
        None,
        Some("injected finalization failure"),
        "finalize connection retirement after CM destruction",
        true,
    );
}

#[test]
fn cm_destroy_barrier_is_budgeted_across_service_passes() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let listener_state = ListenerState::test_only(1);
    let listener = RdmaListener::from_state(&engine.shared.session, Arc::clone(&listener_state));
    let late_listener_state = Arc::clone(&listener_state);
    let mut close = Box::pin(listener.close());
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(close.as_mut().poll(&mut cx).is_pending());
    let destroy_count = Arc::new(AtomicUsize::new(0));
    lock_unpoison(&engine.shared.session.cm.cm_destructions).push_back(
        PendingCmDestruction::Test {
            destroy_count: Arc::clone(&destroy_count),
            target: TestCmDestruction::Listener {
                listener: listener_state,
                destroy_error: None,
            },
        },
    );
    let mut pending = VecDeque::from(["target", "peer"]);
    let mut routed = Vec::new();
    let mut probes = 0;

    for expected in ["target", "peer"] {
        let processed = engine
            .shared
            .session
            .cm
            .service_cm_destructions(&engine.shared.session, 1, || {
                probes += 1;
                let Some(event) = pending.pop_front() else {
                    return Ok(false);
                };
                routed.push(event);
                Ok(true)
            })
            .unwrap();
        assert_eq!(processed, 1);
        assert_eq!(routed.last().copied(), Some(expected));
        assert_eq!(destroy_count.load(Ordering::Acquire), 0);
        assert_eq!(
            lock_unpoison(&engine.shared.session.cm.cm_destructions).len(),
            1
        );
    }
    let processed = engine
        .shared
        .session
        .cm
        .service_cm_destructions(&engine.shared.session, 1, || {
            probes += 1;
            Ok(false)
        })
        .unwrap();
    assert_eq!(processed, 1);
    assert_eq!(probes, 3);
    assert_eq!(routed, ["target", "peer"]);
    assert!(pending.is_empty());
    assert_eq!(destroy_count.load(Ordering::Acquire), 1);
    assert!(lock_unpoison(&engine.shared.session.cm.cm_destructions).is_empty());
    assert!(matches!(close.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    late_listener_state.finish_close(Some(Error::InvalidConfig("late duplicate finish".into())));
    let mut repeated_close = Box::pin(listener.close());
    assert!(matches!(
        repeated_close.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(
        engine
            .shared
            .session
            .cm
            .service_cm_destructions(&engine.shared.session, 1, || Ok(false))
            .unwrap(),
        0
    );
    assert_eq!(destroy_count.load(Ordering::Acquire), 1);
    drop(engine);
    drop(driver);
}

fn selected_accept(listener: &Arc<ListenerState>, route: u64) -> Arc<AcceptRequest> {
    let request = AcceptRequest::test_only();
    listener.register_waiter(Arc::clone(&request)).unwrap();
    assert!(
        listener
            .admit_child(IncomingChild::test_only())
            .rejected
            .is_none()
    );
    let ListenerAction::ProcessSelected {
        request: selected, ..
    } = listener.next_action()
    else {
        panic!("accept was not selected");
    };
    assert!(Arc::ptr_eq(&selected, &request));
    listener.route_selected(&request, route).unwrap();
    request
}

fn assert_connection_cm_destruction_failure(
    destroy_error: Option<&str>,
    finalize_error: Option<&str>,
    expected: &str,
    registry_retained: bool,
) {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let connection = install_connection(
        &engine.shared.session,
        Arc::new(NoopPoster(32)),
        RdmaConnectionConfig::default()
            .max_send_wr(1)
            .max_recv_wr(1),
        None,
        None,
    )
    .unwrap();
    let listener = ListenerState::test_only(1);
    let route = 0x0000_0001_0000_0001;
    let request = selected_accept(&listener, route);
    let completion = InboundRetirementCompletion {
        listener: Arc::downgrade(&listener),
        route,
        request: Some(Arc::clone(&request)),
        result: Some(Error::TransportClosed),
        selected: true,
    };
    let destroy_count = Arc::new(AtomicUsize::new(0));
    lock_unpoison(&engine.shared.session.cm.cm_destructions).push_back(
        PendingCmDestruction::Test {
            destroy_count: Arc::clone(&destroy_count),
            target: TestCmDestruction::Connection {
                connection: Arc::clone(&connection.state),
                completion: Some(completion),
                destroy_error: destroy_error.map(str::to_owned),
                finalize_error: finalize_error.map(str::to_owned),
            },
        },
    );

    let error = engine
        .shared
        .session
        .cm
        .service_cm_destructions(&engine.shared.session, 1, || Ok(false))
        .unwrap_err();
    assert!(error.to_string().contains(expected));
    assert!(connection.state.is_retired());
    let Some(Err(request_error)) = request.take_result_for_test() else {
        panic!("inbound accept must fail before propagation");
    };
    assert!(request_error.to_string().contains(expected));
    let mut close = Box::pin(connection.close());
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let Poll::Ready(Err(close_error)) = close.as_mut().poll(&mut cx) else {
        panic!("connection retirement was not completed before propagation");
    };
    assert!(close_error.to_string().contains(expected));
    assert_eq!(destroy_count.load(Ordering::Acquire), 1);
    assert!(lock_unpoison(&engine.shared.session.cm.cm_destructions).is_empty());
    assert_eq!(
        engine
            .shared
            .session
            .cm
            .service_cm_destructions(&engine.shared.session, 1, || Ok(false))
            .unwrap(),
        0
    );
    assert_eq!(destroy_count.load(Ordering::Acquire), 1);

    let retained = engine
        .shared
        .session
        .connections
        .release(connection.state.token, connection.state.qp_num());
    assert_eq!(retained.is_some(), registry_retained);
    drop(close);
    drop(connection);
    drop(engine);
    drop(driver);
}

#[test]
fn request_failure_and_shutdown_cancellation_keep_first_outcome() {
    let (engine, driver) =
        super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
    let failed = test_request();
    failed.complete_failure(Error::InvalidConfig("first failure".into()));
    failed.complete_failure(Error::InvalidConfig("duplicate failure".into()));
    let cancelled = test_request();
    cancelled.cancel(Error::DriverShutdown);
    cancelled.complete_failure(Error::InvalidConfig("late failure after shutdown".into()));

    drop(engine);
    drop(driver);
}

fn recording_setup(
    order: Arc<Mutex<Vec<&'static str>>>,
    result: Result<SetupSummary>,
) -> ConnectionSetup {
    Box::new(move |_connection, _events| {
        lock_unpoison(&order).push("setup");
        result.map(|summary| summary.posted_wrs)
    })
}

struct NoopPoster(u32);

impl WorkRequestPoster for NoopPoster {
    fn qp_num(&self) -> u32 {
        self.0
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
        Ok(())
    }

    fn destroy_qp(&self) -> Result<bool> {
        Ok(false)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn disconnect(&self) -> Result<()> {
        Ok(())
    }
}

fn test_request() -> OutboundRequest {
    let pool = super::super::connection::ConnectionAdmissionPool::new(1);
    OutboundRequest::new(
        "127.0.0.1:1".parse().unwrap(),
        RdmaConnectionConfig::default(),
        empty_connection_setup(),
        pool.try_acquire().unwrap(),
    )
}
