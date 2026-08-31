#!/bin/bash

set -u

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-}"
primary_status=0
primary_step=""
restoration_status=0
RUN_FULL_WORKSPACE=0

if [[ -z "${CARGO:-}" ]]; then
    if command -v cargo >/dev/null 2>&1; then
        CARGO="$(command -v cargo)"
    elif [[ -n "${SUDO_USER:-}" ]]; then
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        CARGO="$user_home/.rustup/toolchains/stable-aarch64-unknown-linux-gnu/bin/cargo"
    else
        CARGO="$HOME/.rustup/toolchains/stable-aarch64-unknown-linux-gnu/bin/cargo"
    fi
fi
TOOLCHAIN_BIN="$(dirname "$CARGO")"

case "$MODE" in
    --provider-probe)
        TEST_TARGET="v2_engine_provider_probe"
        REPETITIONS=1
        MODE_LABEL="Phase 1 provider probe"
        ;;
    --readiness-race)
        TEST_TARGET="v2_engine_readiness_race"
        REPETITIONS=5
        MODE_LABEL="Phase 2 readiness race"
        ;;
    --driver-flush-gate)
        TEST_TARGET="v2_engine_driver_flush_gate"
        REPETITIONS=1
        MODE_LABEL="Phase 2 driver flush gate"
        ;;
    --phase3-operations)
        TEST_TARGET="v2_engine_operation_tests"
        REPETITIONS=1
        MODE_LABEL="Phase 3 owned-operation routing"
        ;;
    --phase4-connections)
        TEST_TARGET="v2_engine_connection_tests"
        REPETITIONS=1
        MODE_LABEL="Phase 4 shared-CM outbound connections"
        ;;
    --phase5-listeners)
        TEST_TARGET="v2_engine_listener_tests"
        REPETITIONS=1
        MODE_LABEL="Phase 5 listener backlog and accept arbitration"
        ;;
    --phase6-lifecycle)
        TEST_TARGET="v2_engine_lifecycle_tests"
        REPETITIONS=1
        MODE_LABEL="Phase 6 accepted-WR drain and shutdown lifecycle"
        ;;
    --phase7-message-setup)
        TEST_TARGET="v2_engine_message_setup_tests"
        REPETITIONS=1
        MODE_LABEL="Phase 7 engine message setup"
        ;;
    --phase8-message)
        TEST_TARGET="v2_engine_message_tests"
        REPETITIONS=1
        MODE_LABEL="Phase 8 engine message DATA/CREDIT progress"
        RUN_FULL_WORKSPACE=1
        ;;
    *)
        echo "usage: sudo -E ./scripts/validate-v2-engine-providers.sh {--provider-probe|--readiness-race|--driver-flush-gate|--phase3-operations|--phase4-connections|--phase5-listeners|--phase6-lifecycle|--phase7-message-setup|--phase8-message}" >&2
        exit 2
        ;;
esac
if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    echo "provider validation must run as root" >&2
    exit 2
fi
if [[ ! -x "$CARGO" ]]; then
    echo "cargo not found at $CARGO" >&2
    exit 127
fi

record_failure() {
    local status="$1"
    local step="$2"
    if [[ "$primary_status" -eq 0 ]]; then
        primary_status="$status"
        primary_step="$step"
    fi
}

run_step() {
    local step="$1"
    shift
    echo "=== $step ==="
    "$@"
    local status=$?
    if [[ "$status" -ne 0 ]]; then
        record_failure "$status" "$step"
    fi
    return 0
}

run_selected_test() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RUST_TEST_THREADS=1 \
            "$CARGO" test -p rdma-io-tests --test "$TEST_TARGET" -- --nocapture
    else
        env \
            PATH="$TOOLCHAIN_BIN:$PATH" \
            RUST_TEST_THREADS=1 \
            "$CARGO" test -p rdma-io-tests --test "$TEST_TARGET" -- --nocapture
    fi
}

run_provider_test() {
    local provider="$1"
    local iteration
    for ((iteration = 1; iteration <= REPETITIONS; iteration++)); do
        run_step \
            "Run $provider $MODE_LABEL ($iteration/$REPETITIONS)" \
            run_selected_test
    done
    if [[ "$RUN_FULL_WORKSPACE" -eq 1 ]]; then
        run_step \
            "Run $provider full workspace with all features" \
            run_full_workspace
    fi
}

run_full_workspace() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RUST_TEST_THREADS=1 \
            "$CARGO" test --workspace --all-features
    else
        env \
            PATH="$TOOLCHAIN_BIN:$PATH" \
            RUST_TEST_THREADS=1 \
            "$CARGO" test --workspace --all-features
    fi
}

restore_rxe() {
    echo "=== Restoring RXE ==="
    "$ROOT_DIR/scripts/teardown-siw.sh" || restoration_status=$?
    "$ROOT_DIR/scripts/setup-rxe.sh" || restoration_status=$?

    if ! command -v ibv_devices >/dev/null 2>&1; then
        restoration_status=127
    else
        local devices
        devices=$(ibv_devices 2>/dev/null)
        local status=$?
        if [[ "$status" -ne 0 ]]; then
            restoration_status="$status"
        elif ! grep -E -q "rxe0" <<<"$devices"; then
            restoration_status=1
        elif grep -E -q "siw0" <<<"$devices"; then
            restoration_status=1
        fi
    fi
}

trap restore_rxe EXIT

cd "$ROOT_DIR" || exit 1
run_step "Unload hardware RDMA providers" "$ROOT_DIR/scripts/unload-hw-rdma.sh"
run_step "Remove SIW before RXE validation" "$ROOT_DIR/scripts/teardown-siw.sh"
run_step "Set up RXE" "$ROOT_DIR/scripts/setup-rxe.sh"
run_provider_test "RXE"

run_step "Remove RXE before SIW validation" "$ROOT_DIR/scripts/teardown-rxe.sh"
run_step "Set up SIW" "$ROOT_DIR/scripts/setup-siw.sh"
run_provider_test "SIW"

restore_rxe
trap - EXIT

if [[ "$primary_status" -ne 0 ]]; then
    echo "provider probe failed at: $primary_step (status $primary_status)" >&2
    if [[ "$restoration_status" -ne 0 ]]; then
        echo "RXE restoration also failed (status $restoration_status)" >&2
    fi
    exit "$primary_status"
fi
if [[ "$restoration_status" -ne 0 ]]; then
    echo "provider probes passed, but RXE restoration failed (status $restoration_status)" >&2
    exit "$restoration_status"
fi

echo "$MODE_LABEL passed on RXE and SIW; RXE restored and SIW removed."
