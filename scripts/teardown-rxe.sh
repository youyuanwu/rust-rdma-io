#!/bin/bash
# Teardown Soft-RoCE (rxe) RDMA device and unload kernel modules.
#
# Reverses what setup-rxe.sh does: removes the rxe device, then
# unloads the rdma_rxe kernel module.
#
# Usage: sudo ./scripts/teardown-rxe.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✓${NC} $*"; }
warn() { echo -e "${YELLOW}!${NC} $*"; }
fail() { echo -e "${RED}✗${NC} $*"; }

echo "=== Removing rxe RDMA devices ==="
if command -v rdma &>/dev/null; then
    # Find and remove all rxe devices
    for dev in $(rdma link show 2>/dev/null | grep rxe | awk '{print $2}' | cut -d/ -f1); do
        echo -n "  Removing $dev... "
        if rdma link delete "$dev" 2>/dev/null; then
            ok "removed"
        else
            fail "failed to remove $dev"
        fi
    done
    if ! rdma link show 2>/dev/null | grep -q rxe; then
        ok "No rxe devices remaining"
    fi
else
    warn "rdma tool not found — skipping device removal"
fi

echo ""
echo "=== Unloading kernel modules ==="
if lsmod | grep -q "^rdma_rxe "; then
    echo -n "  Unloading rdma_rxe... "
    if rmmod rdma_rxe 2>/dev/null; then
        ok "unloaded"
    else
        fail "failed (may be in use)"
    fi
else
    ok "rdma_rxe not loaded"
fi

echo ""
ok "rxe teardown complete"
