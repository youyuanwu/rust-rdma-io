#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
FIXTURES="$ROOT/rdma-io-tests/api-fixtures/v2-surface"
LOGS="$ROOT/target/v2-api-fixtures/logs"
MANIFEST="$ROOT/rdma-io-tests/tests/fixtures/v2_api_fixture_manifest.tsv"
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

if [[ ! -f "$MANIFEST" ]]; then
    echo "missing API fixture manifest: $MANIFEST" >&2
    exit 1
fi

run_positive() {
    local fixture="$1"
    local binary="$2"
    local manifest="$FIXTURES/$fixture/Cargo.toml"
    CARGO_TARGET_DIR="$ROOT/target/v2-api-fixtures/$fixture/positive" \
        "$CARGO" check --quiet --manifest-path "$manifest" --bin "$binary"
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
    local expected_message="$5"
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

    local error_count
    error_count="$(grep -c ': error\[E[0-9]\{4\}\]:' "$stderr" || true)"
    if [[ "$error_count" -ne 1 ]]; then
        echo "negative fixture must emit exactly one rustc diagnostic: $fixture/$binary" >&2
        cat "$stderr" >&2
        exit 1
    fi

    local error_line
    error_line="$(grep -F "src/bin/$binary.rs:" "$stderr" | grep -F "error[$diagnostic]:" || true)"
    if [[ -z "$error_line" ]]; then
        echo "negative fixture missed exact source-bound diagnostic $diagnostic: $fixture/$binary" >&2
        cat "$stderr" >&2
        exit 1
    fi
    if [[ "$error_line" != *"$symbol"* || "$error_line" != *"$expected_message"* ]]; then
        echo "negative fixture diagnostic did not identify exact removed symbol '$symbol': $fixture/$binary" >&2
        cat "$stderr" >&2
        exit 1
    fi
}

while IFS='|' read -r kind fixture binary diagnostic symbol expected_message; do
    [[ -n "$kind" ]] || continue
    case "$kind" in
        positive)
            run_positive "$fixture" "$binary"
            ;;
        negative)
            run_negative "$fixture" "$binary" "$diagnostic" "$symbol" "$expected_message"
            ;;
        *)
            echo "invalid API fixture manifest kind '$kind'" >&2
            exit 1
            ;;
    esac
done <"$MANIFEST"

cleanup
trap - EXIT
for lock in "${locks[@]}"; do
    if [[ -e "$lock" ]]; then
        echo "fixture lockfile remained after cleanup: $lock" >&2
        exit 1
    fi
done

echo "V2 API surface fixtures passed with diagnostic-bound removals and clean lockfiles."
