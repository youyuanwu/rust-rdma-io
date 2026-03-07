#!/bin/bash
# Setup and verify Soft-RoCE (rxe) kernel module for RDMA testing.
#
# If rdma_rxe.ko is not shipped with the running kernel, the script
# looks for an out-of-tree build produced by cmake (BuildRxe.cmake).
#
# Usage: sudo ./scripts/setup-rxe.sh
#        ./scripts/setup-rxe.sh --check   # check-only, no changes

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✓${NC} $*"; }
warn() { echo -e "${YELLOW}!${NC} $*"; }
fail() { echo -e "${RED}✗${NC} $*"; }

CHECK_ONLY=false
[[ "${1:-}" == "--check" ]] && CHECK_ONLY=true

errors=0

# --- 1. Kernel module availability ---
echo "=== Kernel modules ==="
KVER=$(uname -r)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Look for rdma_rxe in the standard module tree first, then in the cmake
# out-of-tree build directory.
RXE_KO=$(find "/lib/modules/$KVER" -name 'rdma_rxe.ko*' 2>/dev/null | head -1)
RXE_OOT=""

if [[ -z "$RXE_KO" ]]; then
    # Search cmake build dirs for an out-of-tree build
    RXE_OOT=$(find "$PROJECT_DIR/build" -name 'rdma_rxe.ko' 2>/dev/null | head -1)
fi

if [[ -n "$RXE_KO" ]]; then
    ok "rdma_rxe module available in kernel tree ($KVER)"
elif [[ -n "$RXE_OOT" ]]; then
    ok "rdma_rxe module available (out-of-tree build: $RXE_OOT)"
else
    fail "rdma_rxe module not found for kernel $KVER"
    warn "Build out-of-tree with: cmake -B build -DBUILD_RXE=ON && cmake --build build --target rxe"
    ((errors++))
fi

for mod in rdma_cm rdma_ucm ib_uverbs; do
    if lsmod | grep -q "^${mod} "; then
        ok "$mod loaded"
    elif $CHECK_ONLY; then
        warn "$mod not loaded"
    else
        echo -n "  Loading $mod... "
        if modprobe "$mod" 2>/dev/null; then
            ok "loaded"
        else
            fail "failed to load $mod"
            ((errors++))
        fi
    fi
done

# Load rdma_rxe
if lsmod | grep -q "^rdma_rxe "; then
    ok "rdma_rxe loaded"
elif $CHECK_ONLY; then
    warn "rdma_rxe not loaded"
elif [[ -n "$RXE_KO" ]]; then
    echo -n "  Loading rdma_rxe (modprobe)... "
    if modprobe rdma_rxe 2>/dev/null; then
        ok "loaded"
    else
        fail "failed to load rdma_rxe"
        ((errors++))
    fi
elif [[ -n "$RXE_OOT" ]]; then
    echo -n "  Loading rdma_rxe (insmod $RXE_OOT)... "
    if insmod "$RXE_OOT" 2>/dev/null; then
        ok "loaded"
    else
        fail "failed to insmod rdma_rxe"
        ((errors++))
    fi
fi

# --- 2. rxe device ---
echo ""
echo "=== RDMA devices ==="
if ! command -v rdma &>/dev/null; then
    fail "rdma tool not found (install iproute2 or rdma-core)"
    ((errors++))
elif rdma link show 2>/dev/null | grep -q rxe; then
    RXE_DEV=$(rdma link show 2>/dev/null | grep rxe | awk '{print $2}' | cut -d/ -f1)
    ok "rxe device present: $RXE_DEV"
elif $CHECK_ONLY; then
    warn "no rxe device"
else
    # Find first non-lo interface
    IFACE=$(ip -o link show up | awk -F': ' '{print $2}' | grep -v lo | head -1)
    if [[ -z "$IFACE" ]]; then
        fail "no suitable network interface found"
        ((errors++))
    else
        echo -n "  Adding rxe0 on $IFACE... "
        if rdma link add rxe0 type rxe netdev "$IFACE" 2>/dev/null; then
            ok "created"
        else
            fail "failed (may already exist or rdma_rxe not loaded)"
            ((errors++))
        fi
    fi
fi

# --- 3. ibv_devices ---
echo ""
echo "=== ibv_devices ==="
if command -v ibv_devices &>/dev/null; then
    if ibv_devices 2>/dev/null | grep -q rxe; then
        ok "rxe visible to libibverbs"
        ibv_devices 2>/dev/null | grep -v "^$" | sed 's/^/  /'
    else
        fail "rxe not visible to libibverbs"
        ((errors++))
    fi
else
    fail "ibv_devices not found (install libibverbs-utils)"
    ((errors++))
fi

# --- 4. rdma-core libs ---
echo ""
echo "=== Libraries ==="
for lib in libibverbs librdmacm; do
    if ldconfig -p 2>/dev/null | grep -q "$lib"; then
        ok "$lib present"
    else
        fail "$lib not found"
        ((errors++))
    fi
done

# --- Summary ---
echo ""
if [[ $errors -eq 0 ]]; then
    ok "All checks passed — ready for RDMA testing with RXE"
else
    fail "$errors issue(s) found"
    exit 1
fi
