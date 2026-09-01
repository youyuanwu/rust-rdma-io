#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
FIXTURES="$ROOT/rdma-io-tests/api-fixtures/v2-surface"
LOGS="$ROOT/target/v2-api-fixtures/logs"
mkdir -p "$LOGS"

locks=(
    "$FIXTURES/production/Cargo.lock"
    "$FIXTURES/hooks/Cargo.lock"
    "$FIXTURES/no-hooks/Cargo.lock"
)

cleanup() {
    rm -f "${locks[@]}"
}
trap cleanup EXIT

for lock in "${locks[@]}"; do
    manifest="$(dirname "$lock")/Cargo.toml"
    if [[ ! -f "$manifest" ]]; then
        echo "missing fixture manifest: $manifest" >&2
        exit 1
    fi
    if [[ -e "$lock" ]]; then
        echo "fixture lockfile residue exists before validation: $lock" >&2
        exit 1
    fi
    set +e
    tracked="$(
        cd "$ROOT"
        git ls-files -- "${lock#"$ROOT/"}"
    )"
    status=$?
    set -e
    if [[ "$status" -ne 0 || -n "$tracked" ]]; then
        echo "fixture lockfile is tracked or git inspection failed: $lock" >&2
        exit 1
    fi
done

run_positive() {
    local fixture="$1"
    local manifest="$FIXTURES/$fixture/Cargo.toml"
    CARGO_TARGET_DIR="$ROOT/target/v2-api-fixtures/$fixture/positive" \
        "$CARGO" check --quiet --manifest-path "$manifest" --bin positive
    local lock="$FIXTURES/$fixture/Cargo.lock"
    if [[ ! -f "$lock" ]]; then
        echo "fixture did not create its standalone lockfile: $fixture" >&2
        exit 1
    fi
    set +e
    git -C "$ROOT" check-ignore -q "${lock#"$ROOT/"}"
    ignore_status=$?
    set -e
    if [[ "$ignore_status" -ne 0 ]]; then
        echo "fixture lockfile is not covered by the exact ignore rule: $lock" >&2
        exit 1
    fi
}

run_negative() {
    local fixture="$1"
    local binary="$2"
    local diagnostic="$3"
    local symbol="$4"
    local manifest="$FIXTURES/$fixture/Cargo.toml"
    local stderr="$LOGS/$fixture-$binary.stderr"

    set +e
    CARGO_TARGET_DIR="$ROOT/target/v2-api-fixtures/$fixture/$binary" \
        "$CARGO" check --quiet --message-format=short \
        --manifest-path "$manifest" --bin "$binary" 2>"$stderr"
    status=$?
    set -e
    if [[ "$status" -eq 0 ]]; then
        echo "negative fixture unexpectedly compiled: $fixture/$binary" >&2
        exit 1
    fi

    set +e
    grep -F "error[$diagnostic]" "$stderr" >/dev/null
    diagnostic_status=$?
    set -e
    if [[ "$diagnostic_status" -ne 0 ]]; then
        echo "negative fixture missed diagnostic $diagnostic: $fixture/$binary" >&2
        cat "$stderr" >&2
        exit 1
    fi
    set +e
    grep -F "$symbol" "$stderr" >/dev/null
    symbol_status=$?
    set -e
    if [[ "$symbol_status" -ne 0 ]]; then
        echo "negative fixture diagnostic missed symbol '$symbol': $fixture/$binary" >&2
        cat "$stderr" >&2
        exit 1
    fi
}

run_positive production
run_positive hooks
run_positive no-hooks

while IFS='|' read -r fixture binary diagnostic symbol; do
    [[ -n "$fixture" ]] || continue
    run_negative "$fixture" "$binary" "$diagnostic" "$symbol"
done <<'CASES'
production|context_from_cm|E0599|from_cm
production|context_from_inner|E0599|from_inner
production|context_inner|E0599|inner
production|pd_inner|E0599|inner
production|pd_context|E0599|context
production|mr_inner|E0599|inner
production|mr_inner_mut|E0599|inner_mut
production|remote_from_v1|E0599|from_v1
production|remote_to_v1|E0599|to_v1
production|error_from_v1|E0277|rdma_io::v2::Error
production|access_flags|E0624|to_flags
production|cq_work_completion|E0308|WorkCompletion
production|cq_inner|E0599|inner
production|cq_channel|E0599|channel
production|completion_as_wc|E0599|as_wc
production|completion_from_wc_slice|E0599|from_wc_slice
production|completion_from_wc_slice_mut|E0599|from_wc_slice_mut
production|completions_poll_next|E0599|poll_next
production|poller_into_cq|E0599|into_cq
production|qp_attr|E0599|attr
production|qp_from_cm_qp|E0599|from_cm_qp
production|qp_inner|E0599|inner
production|op_import|E0432|Op
production|qp_submit|E0599|submit
production|qp_check_completion|E0599|check_completion
production|protocol_module|E0603|protocol
production|config_send_wr_getter|E0599|maximum_send_work_requests
production|config_recv_wr_getter|E0599|maximum_receive_work_requests
production|config_send_sge_getter|E0599|maximum_send_sges
production|config_recv_sge_getter|E0599|maximum_receive_sges
production|config_responder_getter|E0599|responder_resource_count
production|config_initiator_getter|E0599|initiator_depth_count
production|config_retry_getter|E0599|retry_count_value
production|config_rnr_getter|E0599|rnr_retry_count_value
production|transport_buffer_size|E0599|buffer_size
production|module_context|E0603|context
production|module_cq|E0603|cq
production|module_error|E0603|error
production|module_mr|E0603|mr
production|module_op|E0603|op
production|module_pd|E0603|pd
production|module_qp|E0603|qp
production|module_completion|E0603|completion
production|module_cq_poller|E0603|cq_poller
production|module_engine|E0603|engine
production|module_message|E0603|message_transport
production|module_protocol|E0603|protocol
hooks|root_hook_path|E0603|test_support
hooks|pass_through_hook_path|E0432|engine_driver
hooks|engine_hook_path|E0603|engine
hooks|message_hook_path|E0603|message_transport
no-hooks|test_support|E0432|test_support
CASES

cleanup
trap - EXIT
for lock in "${locks[@]}"; do
    if [[ -e "$lock" ]]; then
        echo "fixture lockfile remained after cleanup: $lock" >&2
        exit 1
    fi
done

echo "V2 API surface fixtures passed with diagnostic-bound removals and clean lockfiles."
