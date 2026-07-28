#!/bin/bash
# Unload hardware RDMA providers so that only software RDMA (siw/rxe) is
# visible to rdma_cm.
#
# Intended for disposable CI machines. Some GitHub-hosted runners expose the
# host NIC's RDMA function — Azure MANA (`mana_ib` → `mana_0`/`manae_0`) or a
# Mellanox VF (`mlx5_ib`) — on the *same* netdev the software device is created
# on. `rdma_cm` resolves an address to the first registered device matching that
# netdev's GID, which is the hardware device, so a same-host test handshake is
# attempted over hardware that cannot complete it: every `rdma_accept` fails
# with `EPROTO` (errno 71) and the peer sees `Rejected`. Removing the hardware
# RDMA drivers (the netdev drivers are left alone, so networking is unaffected)
# leaves siw/rxe as the only candidates.
#
# Usage: sudo ./scripts/unload-hw-rdma.sh
#
# Never fails the caller: on a machine where a provider cannot be unloaded the
# remaining devices are reported so the cause is visible in the job log.

set -uo pipefail

GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✓${NC} $*"; }
warn() { echo -e "${YELLOW}!${NC} $*"; }

# RDMA drivers for physical/paravirtual NICs. `mana` / `mlx5_core` etc. are
# deliberately absent: those drive the netdev and must stay loaded.
HW_RDMA_MODULES=(mana_ib mlx5_ib mlx4_ib irdma i40iw bnxt_re qedr efa)

echo "=== Unloading hardware RDMA providers ==="
for mod in "${HW_RDMA_MODULES[@]}"; do
    lsmod | grep -q "^${mod} " || continue
    echo -n "  Unloading $mod... "
    if modprobe -r "$mod" 2>/dev/null; then
        ok "unloaded"
    else
        warn "failed (module in use?)"
    fi
done

echo ""
echo "=== Remaining RDMA devices ==="
remaining=""
if [[ -d /sys/class/infiniband ]]; then
    for dev in /sys/class/infiniband/*; do
        [[ -e "$dev" ]] || continue
        name=$(basename "$dev")
        case "$name" in
            siw*|rxe*) ok "$name (software)" ;;
            *) warn "$name (hardware)"; remaining="$remaining $name" ;;
        esac
    done
else
    warn "no /sys/class/infiniband — no RDMA devices present"
fi

if [[ -n "$remaining" ]]; then
    warn "hardware RDMA device(s) still present:$remaining"
    warn "rdma_cm may bind test connections to them instead of siw/rxe"
fi

exit 0
