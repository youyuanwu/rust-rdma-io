#!/bin/bash

set -u

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-}"
primary_status=0
primary_step=""
restoration_status=0
RUN_FULL_WORKSPACE=0
ENGINE_CONFORMANCE=0
FULL_VALIDATION=0

if [[ -z "${CARGO:-}" ]]; then
    if command -v cargo >/dev/null 2>&1; then
        CARGO="$(command -v cargo)"
    elif [[ -n "${SUDO_USER:-}" ]]; then
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        if [[ -x "$user_home/.cargo/bin/cargo" ]]; then
            CARGO="$user_home/.cargo/bin/cargo"
        else
            CARGO="$(find "$user_home/.rustup/toolchains" -path '*/bin/cargo' -type f -print -quit 2>/dev/null)"
        fi
    else
        CARGO="$HOME/.cargo/bin/cargo"
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
    --engine-conformance)
        TEST_TARGET=""
        REPETITIONS=1
        MODE_LABEL="Phase 10 engine conformance"
        ENGINE_CONFORMANCE=1
        ;;
    "")
        TEST_TARGET=""
        REPETITIONS=1
        MODE_LABEL="Phase 12 full v2 engine validation"
        FULL_VALIDATION=1
        ;;
    *)
        echo "usage: sudo -E ./scripts/validate-v2-engine-providers.sh [--provider-probe|--readiness-race|--driver-flush-gate|--phase3-operations|--phase4-connections|--phase5-listeners|--phase6-lifecycle|--phase7-message-setup|--phase8-message|--engine-conformance]" >&2
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
    local target="${1:-$TEST_TARGET}"
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RDMA_REQUIRE_PROVIDER=1 \
            RUST_TEST_THREADS=1 \
            "$CARGO" test -p rdma-io-tests --test "$target" -- --nocapture
    else
        env \
            PATH="$TOOLCHAIN_BIN:$PATH" \
            RDMA_REQUIRE_PROVIDER=1 \
            RUST_TEST_THREADS=1 \
            "$CARGO" test -p rdma-io-tests --test "$target" -- --nocapture
    fi
}

run_provider_test() {
    local provider="$1"
    local iteration
    if [[ "$FULL_VALIDATION" -eq 1 ]]; then
        run_step "Build $provider production rdma-io without test-hooks" run_production_build
        run_step "Run $provider Phase 1 provider probe" run_selected_test v2_engine_provider_probe
        run_step "Run $provider Phase 2 driver flush gate" run_selected_test v2_engine_driver_flush_gate
        for ((iteration = 1; iteration <= 5; iteration++)); do
            run_step \
                "Run $provider Phase 2 readiness race ($iteration/5)" \
                run_selected_test v2_engine_readiness_race
        done
        run_step "Run $provider engine resource suite" run_selected_test v2_resource_tests
        run_step "Run $provider engine operation suite" run_selected_test v2_engine_operation_tests
        run_step "Run $provider engine connection suite" run_selected_test v2_engine_connection_tests
        run_step "Run $provider engine listener suite" run_selected_test v2_engine_listener_tests
        run_step "Run $provider engine lifecycle suite" run_selected_test v2_engine_lifecycle_tests
        run_step "Run $provider engine message setup suite" run_selected_test v2_engine_message_setup_tests
        run_step "Run $provider engine message suite" run_selected_test v2_engine_message_tests
        run_step "Run $provider engine diagnostics suite" run_selected_test v2_engine_diagnostics_tests
        run_step "Run $provider engine scaling suite" run_selected_test v2_engine_scaling_tests
        run_step "Run $provider eight-connection conformance" run_selected_test v2_engine_tests
        run_step "Run $provider full workspace with all features" run_full_workspace
        return
    fi
    if [[ "$ENGINE_CONFORMANCE" -eq 1 ]]; then
        run_step "Run $provider Phase 2 driver flush gate" run_selected_test v2_engine_driver_flush_gate
        for ((iteration = 1; iteration <= 5; iteration++)); do
            run_step \
                "Run $provider Phase 2 readiness race ($iteration/5)" \
                run_selected_test v2_engine_readiness_race
        done
        run_step "Run $provider lifecycle/drop composite coverage" run_selected_test v2_engine_lifecycle_tests
        run_step "Run $provider driver-withholding composite coverage" run_selected_test v2_engine_message_setup_tests
        run_step "Run $provider Phase 10 eight-connection conformance" run_selected_test v2_engine_tests
        run_step "Run $provider Phase 10 diagnostics" run_selected_test v2_engine_diagnostics_tests
        run_step "Run $provider Phase 10 scaling" run_selected_test v2_engine_scaling_tests
        return
    fi
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

run_production_build() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            "$CARGO" build -p rdma-io --release --no-default-features --features tokio
    else
        env \
            PATH="$TOOLCHAIN_BIN:$PATH" \
            "$CARGO" build -p rdma-io --release --no-default-features --features tokio
    fi
}

run_full_workspace() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RDMA_REQUIRE_PROVIDER=1 \
            RUST_TEST_THREADS=1 \
            "$CARGO" test --workspace --all-features
    else
        env \
            PATH="$TOOLCHAIN_BIN:$PATH" \
            RDMA_REQUIRE_PROVIDER=1 \
            RUST_TEST_THREADS=1 \
            "$CARGO" test --workspace --all-features
    fi
}

run_static_preflight() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RUSTFLAGS="-D warnings" \
            "$CARGO" check -p rdma-io --no-default-features || return $?
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RUSTFLAGS="-D warnings" \
            "$CARGO" check -p rdma-io --no-default-features --features tokio || return $?
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            CARGO="$CARGO" \
            "$CARGO" test -p rdma-io-tests \
                --test v2_no_hidden_spawn || return $?
    else
        env PATH="$TOOLCHAIN_BIN:$PATH" RUSTFLAGS="-D warnings" \
            "$CARGO" check -p rdma-io --no-default-features || return $?
        env PATH="$TOOLCHAIN_BIN:$PATH" RUSTFLAGS="-D warnings" \
            "$CARGO" check -p rdma-io --no-default-features --features tokio || return $?
        env PATH="$TOOLCHAIN_BIN:$PATH" CARGO="$CARGO" \
            "$CARGO" test -p rdma-io-tests \
                --test v2_no_hidden_spawn || return $?
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

cd "$ROOT_DIR" || exit 1
if [[ "$FULL_VALIDATION" -eq 1 ]]; then
    echo "=== Run build-profile and no-hidden-spawn preflight ==="
    run_static_preflight
    static_status=$?
    if [[ "$static_status" -ne 0 ]]; then
        echo "Static preflight failed before provider switching (status $static_status)" >&2
        exit "$static_status"
    fi
fi

trap restore_rxe EXIT

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
