#!/bin/bash
# Run Ansible E2E tests
# Usage: ./run_tests.sh [playbook] [extra ansible args]
#        ./tests/e2e/run_tests.sh [playbook]  (from repo root)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

PLAYBOOK=""
EXTRA_ARGS=()

for arg in "$@"; do
    if [[ -z "$PLAYBOOK" && "$arg" == *.yml ]]; then
        PLAYBOOK="$arg"
    else
        EXTRA_ARGS+=("$arg")
    fi
done

# Default playbook
PLAYBOOK="${PLAYBOOK:-playbooks/run_tests.yml}"
# Strip prefix if running from repo root
PLAYBOOK="${PLAYBOOK#tests/e2e/}"

INVENTORY="inventory_local.py"

chmod +x "$INVENTORY"

echo "=== Running RDMA E2E Tests ==="
echo "Playbook: $PLAYBOOK"
echo "Inventory: $INVENTORY"
if [[ ${#EXTRA_ARGS[@]} -gt 0 ]]; then
    echo "Extra args: ${EXTRA_ARGS[*]}"
fi
echo ""

# Check inventory
echo "=== Inventory ==="
./"$INVENTORY" --list | python3 -c "import sys,json; hosts=json.load(sys.stdin).get('vms',{}).get('hosts',[]); [print(f'  {h}') for h in hosts]" 2>/dev/null || echo "  No VMs found"
echo ""

ansible-playbook -i "$INVENTORY" "$PLAYBOOK" -v "${EXTRA_ARGS[@]}"
