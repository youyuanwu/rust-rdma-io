#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
NO_HOOKS_TARGET="$ROOT/target/v2-rustdoc/no-hooks"
ALL_FEATURES_TARGET="$ROOT/target/v2-rustdoc/all-features"

RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR="$NO_HOOKS_TARGET" \
    "$CARGO" doc --quiet -p rdma-io --no-default-features --features tokio --no-deps
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR="$ALL_FEATURES_TARGET" \
    "$CARGO" doc --quiet -p rdma-io --all-features --no-deps

DOC="$NO_HOOKS_TARGET/doc/rdma_io/v2"
if [[ ! -f "$DOC/index.html" ]]; then
    echo "missing rendered V2 rustdoc index: $DOC/index.html" >&2
    exit 1
fi

anchors=(
    "index.html"
    "enum.Error.html"
    "struct.Context.html"
    "struct.Pd.html"
    "enum.AccessIntent.html"
    "struct.Mr.html"
    "struct.RemoteMr.html"
    "struct.CqBuilder.html"
    "struct.Cq.html"
    "struct.Completions.html"
    "type.TokioCompletions.html"
    "struct.CqPoller.html"
    "struct.QpBuilder.html"
    "struct.Qp.html"
    "struct.Completion.html"
    "struct.RdmaEngineBuilder.html"
    "struct.RdmaEngineDiagnostics.html"
    "struct.RdmaConnectionIdentity.html"
)

for anchor in "${anchors[@]}"; do
    page="$DOC/$anchor"
    if [[ ! -f "$page" ]]; then
        echo "missing rendered V2 documentation anchor: $anchor" >&2
        exit 1
    fi
    for marker in "Use case" "Ownership and progress" "Safety and limits" "Availability"; do
        set +e
        grep -F "$marker" "$page" >/dev/null
        marker_status=$?
        set -e
        if [[ "$marker_status" -ne 0 ]]; then
            echo "rendered anchor $anchor is missing marker '$marker'" >&2
            exit 1
        fi
    done
done

for removed_page in \
    context/index.html cq/index.html error/index.html mr/index.html op/index.html \
    pd/index.html qp/index.html completion/index.html cq_poller/index.html \
    engine/index.html message_transport/index.html protocol/index.html \
    struct.Op.html enum.OpCode.html; do
    if [[ -e "$DOC/$removed_page" ]]; then
        echo "removed/internalized V2 rustdoc page remains: $removed_page" >&2
        exit 1
    fi
done

for profile in "$NO_HOOKS_TARGET" "$ALL_FEATURES_TARGET"; do
    set +e
    find "$profile/doc/rdma_io" -type f -name '*.html' -print0 \
        | xargs -0 grep -l 'test_support::engine_driver\|v2::engine::Test\|v2::message_transport::Test' \
        >"$profile/removed-hook-pages.txt"
    grep_status=$?
    set -e
    if [[ "$grep_status" -eq 0 && -s "$profile/removed-hook-pages.txt" ]]; then
        echo "legacy hook path appears in rendered rustdoc:" >&2
        cat "$profile/removed-hook-pages.txt" >&2
        exit 1
    fi
    if [[ "$grep_status" -ne 0 && "$grep_status" -ne 123 ]]; then
        echo "rendered rustdoc hook scan failed with status $grep_status" >&2
        exit "$grep_status"
    fi
done

echo "Stable V2 rustdoc anchors, links, and removed-page checks passed."
