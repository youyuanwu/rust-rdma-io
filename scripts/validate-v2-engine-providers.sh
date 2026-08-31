#!/bin/bash

set -u

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-}"
primary_status=0
primary_step=""
restoration_status=0

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

if [[ "$MODE" != "--provider-probe" ]]; then
    echo "usage: sudo -E ./scripts/validate-v2-engine-providers.sh --provider-probe" >&2
    exit 2
fi
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

run_probe_test() {
    if [[ -n "${SUDO_USER:-}" ]]; then
        local user_home
        user_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        sudo -u "$SUDO_USER" env \
            HOME="$user_home" \
            PATH="$TOOLCHAIN_BIN:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            RUST_TEST_THREADS=1 \
            "$CARGO" test -p rdma-io-tests --test v2_engine_provider_probe -- --nocapture
    else
        env \
            PATH="$TOOLCHAIN_BIN:$PATH" \
            RUST_TEST_THREADS=1 \
            "$CARGO" test -p rdma-io-tests --test v2_engine_provider_probe -- --nocapture
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
run_step "Remove SIW before RXE probe" "$ROOT_DIR/scripts/teardown-siw.sh"
run_step "Set up RXE" "$ROOT_DIR/scripts/setup-rxe.sh"
run_step "Run RXE Phase 1 provider probe" run_probe_test

run_step "Remove RXE before SIW probe" "$ROOT_DIR/scripts/teardown-rxe.sh"
run_step "Set up SIW" "$ROOT_DIR/scripts/setup-siw.sh"
run_step "Run SIW Phase 1 provider probe" run_probe_test

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

echo "Phase 1 provider probes passed on RXE and SIW; RXE restored and SIW removed."
