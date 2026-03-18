#!/bin/bash
# Run RDMA benchmarks
# Usage:
#   ./run_bench.sh --mode tls --connections 4   # Specific config
#   ./run_bench.sh --matrix                     # Full matrix

set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Defaults
MODE=""
CONNECTIONS=""
THREADS=""
DURATION="10"
PAYLOAD="64"
MATRIX=false

# Matrix defaults
MATRIX_MODES="tls"
MATRIX_CONNECTIONS="1 4 16"
MATRIX_THREADS="1 2"

while [[ $# -gt 0 ]]; do
    case $1 in
        --mode)          MODE="$2"; shift 2 ;;
        --connections)   CONNECTIONS="$2"; shift 2 ;;
        --threads)       THREADS="$2"; shift 2 ;;
        --duration)      DURATION="$2"; shift 2 ;;
        --payload)       PAYLOAD="$2"; shift 2 ;;
        --matrix)        MATRIX=true; shift ;;
        --matrix-modes)       MATRIX_MODES="$2"; shift 2 ;;
        --matrix-connections) MATRIX_CONNECTIONS="$2"; shift 2 ;;
        --matrix-threads)     MATRIX_THREADS="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Single run:"
            echo "  --mode <tls|h3>           Protocol mode [default: tls]"
            echo "  --connections <N>          Concurrent connections [default: 1]"
            echo "  --threads <N>             Worker threads [default: 1]"
            echo "  --duration <SECS>         Test duration [default: 10]"
            echo "  --payload <BYTES>         Payload size [default: 64]"
            echo ""
            echo "Matrix run:"
            echo "  --matrix                  Run full matrix"
            echo "  --matrix-modes <LIST>     Modes to test [default: 'tls']"
            echo "  --matrix-connections      Connection counts [default: '1 4 16']"
            echo "  --matrix-threads          Thread counts [default: '1 2']"
            echo ""
            echo "Examples:"
            echo "  $0 --mode tls --connections 4"
            echo "  $0 --matrix"
            echo "  $0 --matrix --matrix-connections '1 4'"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

INVENTORY="inventory_local.py"
chmod +x "$INVENTORY"

run_one() {
    local m="$1" c="$2" th="$3"
    echo "=== mode=$m connections=$c threads=$th ==="
    ansible-playbook -i "$INVENTORY" playbooks/bench_run.yml \
        -e "bench_mode=$m" \
        -e "bench_connections=$c" \
        -e "bench_threads=$th" \
        -e "bench_duration=$DURATION" \
        -e "bench_payload=$PAYLOAD"
}

if [[ "$MATRIX" != "true" ]]; then
    # Single run
    run_one "${MODE:-tls}" "${CONNECTIONS:-1}" "${THREADS:-1}"
else
    # Matrix run
    echo "========================================="
    echo "  RDMA Benchmark Matrix"
    echo "  Modes: $MATRIX_MODES"
    echo "  Connections: $MATRIX_CONNECTIONS"
    echo "  Threads: $MATRIX_THREADS"
    echo "========================================="
    echo ""

    COUNT=0
    for m in $MATRIX_MODES; do
        for c in $MATRIX_CONNECTIONS; do
            for th in $MATRIX_THREADS; do
                run_one "$m" "$c" "$th"
                ((COUNT++))
                echo ""
            done
        done
    done

    echo "========================================="
    echo "  Matrix complete: $COUNT configurations"
    echo "  Results saved to /tmp/bench-*.json"
    echo "========================================="
fi
