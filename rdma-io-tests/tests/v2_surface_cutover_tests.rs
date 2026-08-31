//! Mechanical checks for the final V2 engine surface cutover.

use std::collections::BTreeSet;

const SUPERSEDED_OPERATION_CASES: &[&str] = &[
    "test_shared_qp_send_recv_fd",
    "test_shared_qp_send_recv_polling",
    "test_shared_qp_write_read_fd",
    "test_shared_qp_completion_error",
    "test_inflight_concurrent_registrations",
];

const MOVED_RESOURCE_CASES: &[&str] = &[
    "test_v2_context_and_pd",
    "test_v2_context_open_by_name",
    "test_v2_context_not_found",
];

const RETAINED_LOW_LEVEL_CASES: &[&str] = &[
    "test_v2_cq_poll_only",
    "test_v2_cq_with_channel",
    "test_v2_mr_registration",
    "test_v2_mr_zero_size_rejected",
    "test_v2_builder_defaults",
    "test_v2_send_recv_poll",
    "test_v2_send_recv_async",
    "test_v2_rdma_write_read",
    "test_v2_drop_order",
    "test_v2_access_intent_flags",
    "test_v2_completion_error",
    "test_v2_cq_poller_send_recv",
    "test_v2_op_submit_completion",
    "test_v2_completion_result_error",
];

const V2_MODULE_SOURCE: &str = include_str!("../../rdma-io/src/v2/mod.rs");
const CONNECTION_TEST_SOURCE: &str = include_str!("v2_engine_connection_tests.rs");
const FLUSH_TEST_SOURCE: &str = include_str!("v2_engine_driver_flush_gate.rs");
const REGISTRY_TEST_SOURCE: &str = include_str!("../../rdma-io/src/v2/engine/registry.rs");
const RESOURCE_TEST_SOURCE: &str = include_str!("v2_resource_tests.rs");
const LOW_LEVEL_TEST_SOURCE: &str = include_str!("v2_tests.rs");

#[test]
fn all_five_superseded_operation_cases_have_engine_replacements() {
    assert_eq!(SUPERSEDED_OPERATION_CASES.len(), 5);
    assert_eq!(
        SUPERSEDED_OPERATION_CASES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        5
    );
    for marker in [
        "outbound_connect_and_operations_use_one_shared_engine_in_both_modes",
        "exercise_operations",
    ] {
        assert!(CONNECTION_TEST_SOURCE.contains(marker));
    }
    assert!(
        FLUSH_TEST_SOURCE
            .contains("explicit_qp_err_flushes_every_accepted_wr_in_readiness_and_polling_modes")
    );
    assert!(
        REGISTRY_TEST_SOURCE.contains("concurrent_registrations_fill_and_release_exact_capacity")
    );
}

#[test]
fn all_17_low_level_cases_are_moved_or_retained_exactly_once() {
    assert_eq!(MOVED_RESOURCE_CASES.len(), 3);
    assert_eq!(RETAINED_LOW_LEVEL_CASES.len(), 14);
    let all = MOVED_RESOURCE_CASES
        .iter()
        .chain(RETAINED_LOW_LEVEL_CASES)
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(all.len(), 17);

    for name in MOVED_RESOURCE_CASES {
        assert!(RESOURCE_TEST_SOURCE.contains(&format!("fn {name}(")));
        assert!(!LOW_LEVEL_TEST_SOURCE.contains(&format!("fn {name}(")));
    }
    for name in RETAINED_LOW_LEVEL_CASES {
        assert!(LOW_LEVEL_TEST_SOURCE.contains(&format!("fn {name}(")));
        assert!(!RESOURCE_TEST_SOURCE.contains(&format!("fn {name}(")));
    }
}

#[test]
fn v2_root_exports_only_resources_and_engine_owned_frontends() {
    for removed in [
        concat!("Shared", "Qp"),
        concat!("Op", "Future"),
        concat!("MessageTransport", "Driver"),
        concat!("FdCq", "Driver"),
        concat!("PollingCq", "Driver"),
        concat!("CqDriver", "Handle"),
    ] {
        assert!(
            !V2_MODULE_SOURCE.contains(removed),
            "removed V2 surface remains exported: {removed}"
        );
    }
    for obsolete_module in [
        concat!("mod ", "connection;"),
        concat!("mod ", "inflight;"),
        concat!("mod ", "shared_qp;"),
    ] {
        assert!(
            !V2_MODULE_SOURCE.contains(obsolete_module),
            "obsolete V2 module remains declared: {obsolete_module}"
        );
    }
    for retained in [
        "pub mod context;",
        "pub mod cq;",
        "pub mod mr;",
        "pub mod op;",
        "pub mod pd;",
        "pub mod qp;",
        "pub mod engine;",
        "pub mod message_transport;",
    ] {
        assert!(
            V2_MODULE_SOURCE.contains(retained),
            "retained V2 module is missing: {retained}"
        );
    }
}
