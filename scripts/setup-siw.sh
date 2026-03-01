#!/bin/bash
# Setup and verify SoftIWarp (siw) kernel module for RDMA testing.
#
# Usage: sudo ./scripts/setup-siw.sh
#        ./scripts/setup-siw.sh --check   # check-only, no changes

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
SIW_KO=$(find "/lib/modules/$KVER" -name 'siw.ko*' 2>/dev/null | head -1)

if [[ -n "$SIW_KO" ]]; then
    ok "siw module available ($KVER)"
else
    fail "siw module not found for kernel $KVER"
    EXTRA_PKG="linux-modules-extra-$KVER"
    if apt-cache show "$EXTRA_PKG" &>/dev/null; then
        warn "Install with: sudo apt-get install -y $EXTRA_PKG"
    fi
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

if lsmod | grep -q "^siw "; then
    ok "siw loaded"
elif $CHECK_ONLY; then
    warn "siw not loaded"
elif [[ -n "$SIW_KO" ]]; then
    echo -n "  Loading siw... "
    if modprobe siw 2>/dev/null; then
        ok "loaded"
    else
        fail "failed to load siw"
        ((errors++))
    fi
fi

# --- 2. siw device ---
echo ""
echo "=== RDMA devices ==="
if ! command -v rdma &>/dev/null; then
    fail "rdma tool not found (install iproute2 or rdma-core)"
    ((errors++))
elif rdma link show 2>/dev/null | grep -q siw; then
    SIW_DEV=$(rdma link show 2>/dev/null | grep siw | awk '{print $2}' | cut -d/ -f1)
    ok "siw device present: $SIW_DEV"
elif $CHECK_ONLY; then
    warn "no siw device"
else
    # Find first non-lo interface
    IFACE=$(ip -o link show up | awk -F': ' '{print $2}' | grep -v lo | head -1)
    if [[ -z "$IFACE" ]]; then
        fail "no suitable network interface found"
        ((errors++))
    else
        echo -n "  Adding siw0 on $IFACE... "
        if rdma link add siw0 type siw netdev "$IFACE" 2>/dev/null; then
            ok "created"
        else
            fail "failed (may already exist or siw not loaded)"
            ((errors++))
        fi
    fi
fi

# --- 3. ibv_devices ---
echo ""
echo "=== ibv_devices ==="
if command -v ibv_devices &>/dev/null; then
    if ibv_devices 2>/dev/null | grep -q siw; then
        ok "siw visible to libibverbs"
        ibv_devices 2>/dev/null | grep -v "^$" | sed 's/^/  /'
    else
        fail "siw not visible to libibverbs"
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
    ok "All checks passed — ready for RDMA testing"
else
    fail "$errors issue(s) found"
    exit 1
fi
