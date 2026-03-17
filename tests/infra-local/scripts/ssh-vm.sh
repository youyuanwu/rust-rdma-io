#!/bin/bash
# SSH helper for local RDMA VMs

export LIBVIRT_DEFAULT_URI="qemu:///system"

VM="${1:-1}"
USER="${SSH_USER:-azureuser}"
IP_ONLY=false
WAIT_SECS=0

# Parse flags
shift 2>/dev/null || true
for arg in "$@"; do
    case "$arg" in
        --ip-only) IP_ONLY=true ;;
        --wait=*) WAIT_SECS="${arg#--wait=}" ;;
    esac
done

case "$VM" in
    1|vm1)
        VM_NAME="rdma-vm1"
        ;;
    2|vm2)
        VM_NAME="rdma-vm2"
        ;;
    *)
        echo "Usage: $0 [1|2|vm1|vm2] [--ip-only] [--wait=SECONDS]"
        echo ""
        echo "Options:"
        echo "  --ip-only       Print VM IP and exit"
        echo "  --wait=SECONDS  Wait for SSH + extra seconds (e.g. for cloud-init)"
        echo ""
        echo "Examples:"
        echo "  $0 1                # SSH to VM1"
        echo "  $0 vm2              # SSH to VM2"
        echo "  $0 1 --ip-only      # Print VM1 IP only"
        echo "  $0 1 --wait=600     # Wait up to 10 min for VM1 to be ready"
        exit 1
        ;;
esac

# Total wait budget
TOTAL_WAIT=$((WAIT_SECS > 0 ? WAIT_SECS : 120))

get_vm_ip() {
    virsh net-dhcp-leases default 2>/dev/null | grep "$VM_NAME" | awk '{print $5}' | cut -d'/' -f1
}

IP=$(get_vm_ip)

if [[ -z "$IP" ]]; then
    IP_TIMEOUT=$TOTAL_WAIT
    IP_ATTEMPTS=$((IP_TIMEOUT / 2))
    echo "Waiting for $VM_NAME to get IP address... (timeout ${IP_TIMEOUT}s)" >&2
    for i in $(seq 1 "$IP_ATTEMPTS"); do
        IP=$(get_vm_ip)
        if [[ -n "$IP" ]]; then
            break
        fi
        sleep 2
    done
fi

if [[ -z "$IP" ]]; then
    echo "Error: Could not find IP for $VM_NAME" >&2
    echo "Check if VM is running: virsh list" >&2
    echo "Check DHCP leases: virsh net-dhcp-leases default" >&2
    exit 1
fi

if [[ "$IP_ONLY" == "true" ]]; then
    echo "$IP"
    exit 0
fi

# Wait for cloud-init to complete (polls via SSH, fixed 5 min timeout)
if [[ "$WAIT_SECS" -gt 0 ]]; then
    CI_TIMEOUT=300
    echo "Waiting for cloud-init on $VM_NAME ($IP)... (timeout ${CI_TIMEOUT}s)"
    DEADLINE=$((SECONDS + CI_TIMEOUT))
    while [[ $SECONDS -lt $DEADLINE ]]; do
        if ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no -o BatchMode=yes \
               -o UserKnownHostsFile=/dev/null "$USER@$IP" \
               "cloud-init status 2>/dev/null | grep -q done" 2>/dev/null; then
            echo "cloud-init done on $VM_NAME"
            break
        fi
        sleep 5
    done
    exit 0
fi

# Interactive: just SSH in
echo "Connecting to $VM_NAME ($IP)..."
exec ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "$USER@$IP"
