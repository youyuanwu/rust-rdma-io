#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

export LIBVIRT_DEFAULT_URI="qemu:///system"

# Parse arguments
AUTO_APPROVE=false
NO_COLOR=false

while [[ $# -gt 0 ]]; do
    case $1 in
        -y|--yes)
            AUTO_APPROVE=true
            shift
            ;;
        --no-color)
            NO_COLOR=true
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  -y, --yes   Auto-approve without prompting"
            echo "  --no-color  Disable colored output"
            echo "  -h, --help  Show this help"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

if [[ "$NO_COLOR" == "true" ]]; then
    TF_COLOR_ARG="-no-color"
else
    TF_COLOR_ARG=""
fi

echo "========================================"
echo "  RDMA Local VM Destruction"
echo "========================================"
echo ""

if [[ ! -f "terraform.tfstate" ]]; then
    echo "No terraform state found. Nothing to destroy."
    exit 0
fi

if [[ "$AUTO_APPROVE" == "true" ]]; then
    terraform destroy $TF_COLOR_ARG -auto-approve
else
    terraform destroy $TF_COLOR_ARG
fi

echo ""
echo "VMs destroyed successfully."

# Optionally clean up storage pool
POOL_NAME="rdma-local"
if virsh pool-info "$POOL_NAME" &>/dev/null; then
    echo "Cleaning up storage pool '$POOL_NAME'..."
    virsh pool-destroy "$POOL_NAME" 2>/dev/null || true
    virsh pool-undefine "$POOL_NAME" 2>/dev/null || true
    echo "Storage pool removed."
fi
