//! Mechanical migration ledger for the 61 superseded endpoint message tests.
//!
//! Runtime behavior now lives in the engine setup, message, lifecycle,
//! connection, operation, and no-hidden-work suites. This target keeps the
//! exact one-to-one disposition auditable without duplicating provider-heavy
//! scenarios.

use std::collections::BTreeSet;
use syn::{Attribute, Item};

const MESSAGE_BEHAVIOR: &[&str] = &[
    "test_single_message_readiness",
    "test_multiple_messages_readiness",
    "test_oversize_rejected",
    "test_max_size_message",
    "test_zero_length_message",
    "test_buffer_reuse_beyond_pool",
    "test_single_message_polling",
    "test_multiple_messages_polling",
    "test_oversize_rejected_polling",
    "test_max_size_message_polling",
    "test_zero_length_message_polling",
    "test_buffer_reuse_beyond_pool_polling",
    "test_recv_cancel_no_message_loss",
    "test_send_cancel_buffer_recovered",
    "test_concurrent_sends",
    "test_concurrent_receivers",
    "test_send_backpressure",
    "test_credit_flow_control",
    "test_disconnect_wakes_pending_recv",
    "test_disconnect_wakes_pending_send",
    "test_disconnect_wakes_pending_recv_polling",
    "test_credit_never_exceeds_capacity",
    "test_concurrent_send_abort_no_hang",
    "test_credit_validation_no_false_positive_under_load",
    "test_credit_validation_batch_return",
    "test_credit_validation_concurrent_senders",
];

const SETUP_AND_HELLO: &[&str] = &["test_hello_mismatch_fails_ready"];

const LIFECYCLE_AND_DIAGNOSTICS: &[&str] = &[
    "test_drop_no_hang",
    "test_shutdown_wakes_recv",
    "test_shutdown_wakes_pending_recv",
    "test_close_no_hang",
    "test_drop_no_hang_polling",
    "test_inflight_registry_reclaim",
    "test_drop_unspawned_driver_fails_frontend",
    "test_abort_driver_task_fails_frontend",
    "test_frontend_close_exits_driver",
    "test_frontend_drop_exits_driver",
    "test_close_unspawned_driver_no_hang",
    "test_driver_abort_propagates_to_frontend",
    "test_error_observation_clean_close_no_error",
    "test_error_observation_driver_drop_unspawned",
    "test_error_observation_peer_disconnect_state",
    "test_error_and_driver_result_consistent",
    "test_lifetime_unspawned_driver_dropped_frontend_remains",
    "test_lifetime_spawned_driver_aborted_frontend_remains",
    "test_lifetime_frontend_dropped_driver_remains",
    "test_lifetime_inflight_send_recv_cancellation",
    "test_lifetime_final_owner_drop_order",
    "test_mr_quarantine_on_driver_abort",
    "test_mr_quarantine_on_unspawned_driver_drop",
    "test_graceful_close_drains_real_cqes",
];

const STRUCTURAL_EVIDENCE: &[&str] = &[
    "test_qp_destroy_before_mr_deregistration_order",
    "test_transport_shared_state_field_order",
    "test_inflight_map_close_wakes_waiters",
];

const ENGINE_OWNERSHIP: &[&str] = &[
    "test_shared_cq_single_driver",
    "test_no_progress_without_driver_poll",
    "test_readiness_completes_after_both_drivers",
    "test_one_task_per_endpoint_separate_cq",
    "test_readiness_mode_explicit_spawn",
    "test_polling_mode_explicit_spawn",
    "test_one_task_per_endpoint_shared_cq",
];

const MESSAGE_SOURCE: &str = include_str!("v2_engine_message_tests.rs");
const SETUP_SOURCE: &str = include_str!("v2_engine_message_setup_tests.rs");
const LIFECYCLE_SOURCE: &str = include_str!("v2_engine_lifecycle_tests.rs");
const CONNECTION_SOURCE: &str = include_str!("v2_engine_connection_tests.rs");
const OPERATION_SOURCE: &str = include_str!("v2_engine_operation_tests.rs");
const NO_HIDDEN_WORK_SOURCE: &str = include_str!("v2_no_hidden_spawn.rs");
const MESSAGE_UNIT_SOURCE: &str = include_str!("../../rdma-io/src/v2/message_transport.rs");
const REGISTRY_UNIT_SOURCE: &str = include_str!("../../rdma-io/src/v2/engine/registry.rs");

fn collect_functions(items: &[Item], functions: &mut BTreeSet<(String, bool)>) {
    for item in items {
        match item {
            Item::Fn(function) => {
                functions.insert((
                    function.sig.ident.to_string(),
                    function.attrs.iter().any(is_test_attribute),
                ));
            }
            Item::Mod(module) => {
                if let Some((_, items)) = &module.content {
                    collect_functions(items, functions);
                }
            }
            _ => {}
        }
    }
}

fn is_test_attribute(attribute: &Attribute) -> bool {
    attribute
        .path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "test")
}

fn parsed_functions(source: &str) -> BTreeSet<(String, bool)> {
    let syntax = syn::parse_file(source).expect("replacement evidence source must parse");
    let mut functions = BTreeSet::new();
    collect_functions(&syntax.items, &mut functions);
    functions
}

fn assert_test_functions(source: &str, functions: &[&str]) {
    let parsed = parsed_functions(source);
    for function in functions {
        assert!(
            parsed.contains(&(function.to_string(), true)),
            "replacement evidence is missing test function {function}"
        );
    }
}

fn assert_function(source: &str, function: &str) {
    let parsed = parsed_functions(source);
    assert!(
        parsed.iter().any(|(name, _)| name == function),
        "replacement evidence is missing function {function}"
    );
}

#[test]
fn all_61_message_cases_have_one_engine_disposition_and_live_evidence() {
    assert_eq!(MESSAGE_BEHAVIOR.len(), 26);
    assert_eq!(SETUP_AND_HELLO.len(), 1);
    assert_eq!(LIFECYCLE_AND_DIAGNOSTICS.len(), 24);
    assert_eq!(STRUCTURAL_EVIDENCE.len(), 3);
    assert_eq!(ENGINE_OWNERSHIP.len(), 7);

    let groups = [
        MESSAGE_BEHAVIOR,
        SETUP_AND_HELLO,
        LIFECYCLE_AND_DIAGNOSTICS,
        STRUCTURAL_EVIDENCE,
        ENGINE_OWNERSHIP,
    ];
    let all = groups.into_iter().flatten().copied().collect::<Vec<_>>();
    assert_eq!(all.len(), 61);
    assert_eq!(all.iter().copied().collect::<BTreeSet<_>>().len(), 61);

    assert_test_functions(
        MESSAGE_SOURCE,
        &[
            "data_boundaries_registered_reuse_and_negotiated_credits",
            "received_message_drop_reposts_and_returns_credit_only_in_engine_work",
            "malformed_and_duplicate_control_frames_fail_connection_locally",
            "queued_send_cancellation_and_disconnect_wake_observers",
            "cancelled_recv_does_not_consume_successor_message_in_both_modes",
            "hot_message_work_rotates_and_connection_close_is_independent",
            "cancelled_data_send_reclaims_after_qp_destroy_and_rejects_late_cqe",
        ],
    );
    assert_test_functions(
        SETUP_SOURCE,
        &[
            "readiness_malformed_hello_is_connection_local",
            "polling_malformed_hello_is_connection_local",
            "message_setup_requires_the_owning_driver_to_be_polled",
        ],
    );
    assert_test_functions(
        LIFECYCLE_SOURCE,
        &[
            "clean_close_records_real_qp_mr_cm_and_canonical_ack_order_in_both_modes",
            "omitted_flush_cqe_uses_qp_destroy_before_clean_reclaim_in_both_modes",
            "held_real_cqe_uses_qp_destroy_for_clean_shutdown_in_both_modes",
            "result_aware_qp_destroy_failure_quarantines_mr_and_debt_in_both_modes",
            "dropping_an_unspawned_driver_is_typed_and_consistent_in_both_modes",
            "aborting_the_driver_with_an_accepted_wr_wedges_and_wakes_in_both_modes",
            "peer_disconnect_uses_the_same_explicit_local_qp_err_close_path_in_both_modes",
        ],
    );
    assert_test_functions(
        CONNECTION_SOURCE,
        &[
            "withholding_the_driver_prevents_cm_progress_and_cancellation_releases_admission",
            "outbound_api_surface_has_exact_future_outputs",
        ],
    );
    assert_test_functions(
        OPERATION_SOURCE,
        &["owned_operations_route_by_generation_qp_and_token_in_both_modes"],
    );
    assert_test_functions(NO_HIDDEN_WORK_SOURCE, &["test_no_hidden_spawn_in_v2"]);
    assert_function(NO_HIDDEN_WORK_SOURCE, "collect_rs_files");
    assert_test_functions(
        MESSAGE_UNIT_SOURCE,
        &[
            "engine_credit_returns_are_atomic_capped_and_duplicate_safe",
            "engine_terminal_failure_wakes_ready_recv_and_send_terminal_waiters",
        ],
    );
    assert_test_functions(
        REGISTRY_UNIT_SOURCE,
        &[
            "concurrent_registrations_fill_and_release_exact_capacity",
            "maximum_generation_retires_without_wrapping",
        ],
    );
}
