#!/bin/bash
# Teardown SoftIWarp (siw) RDMA device and unload kernel modules.
#
# Reverses what setup-siw.sh does: removes the siw device, then
# unloads the siw kernel module.
#
# Usage: sudo ./scripts/teardown-siw.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✓${NC} $*"; }
warn() { echo -e "${YELLOW}!${NC} $*"; }
fail() { echo -e "${RED}✗${NC} $*"; }

echo "=== Removing siw RDMA devices ==="
if command -v rdma &>/dev/null; then
    # Find and remove all siw devices
    for dev in $(rdma link show 2>/dev/null | grep siw | awk '{print $2}' | cut -d/ -f1); do
        echo -n "  Removing $dev... "
        if rdma link delete "$dev" 2>/dev/null; then
            ok "removed"
        else
            fail "failed to remove $dev"
        fi
    done
    if ! rdma link show 2>/dev/null | grep -q siw; then
        ok "No siw devices remaining"
    fi
else
    warn "rdma tool not found — skipping device removal"
fi

echo ""
echo "=== Unloading kernel modules ==="
if lsmod | grep -q "^siw "; then
    echo -n "  Unloading siw... "
    if rmmod siw 2>/dev/null; then
        ok "unloaded"
    else
        fail "failed (may be in use)"
    fi
else
    ok "siw not loaded"
fi

echo ""
ok "siw teardown complete"
