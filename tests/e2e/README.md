# E2E Tests with Ansible

End-to-end tests that run against local QEMU/KVM VMs provisioned with Terraform.

## Prerequisites

```bash
sudo apt install -y ansible
```

## Setup

1. Deploy VMs:
   ```bash
   ./tests/infra-local/scripts/start-vm.sh -y
   ```

2. Verify inventory:
   ```bash
   ./tests/e2e/inventory_local.py --list
   ```

## Run Tests

```bash
# Full suite (setup rxe + connectivity + cross-node rping)
./tests/e2e/run_tests.sh

# Individual playbooks
./tests/e2e/run_tests.sh playbooks/hello_world.yml          # Basic connectivity
./tests/e2e/run_tests.sh playbooks/test_connectivity.yml     # Ping between VMs
./tests/e2e/run_tests.sh playbooks/setup_rdma.yml            # Load rxe, verify RDMA
./tests/e2e/run_tests.sh playbooks/rdma_ping_test.yml        # Cross-node rping
```

## Playbooks

| Playbook | Description |
|----------|-------------|
| `hello_world.yml` | Basic connectivity, OS info |
| `test_connectivity.yml` | Ping between VMs |
| `setup_rdma.yml` | Load rxe module, create rxe0 device, verify with loopback rping |
| `rdma_ping_test.yml` | Cross-node rping (server on VM1, client on VM2) |
| `run_tests.yml` | Full suite: setup + connectivity + rping |

## Dynamic Inventory

The `inventory_local.py` script reads VM IPs from libvirt DHCP leases.

## Adding New Tests

Create a new playbook in `playbooks/` directory:

```yaml
---
- name: My Test
  hosts: vms
  tasks:
    - name: Run something
      ansible.builtin.shell: echo "test"
```

## RDMA Setup Details

The `setup_rdma.yml` playbook handles rxe module loading:

1. Tries `modprobe rdma_rxe` (works if VM kernel ships it)
2. If that fails, copies host-built `rdma_rxe.ko` from `build/` and uses `insmod`
3. Auto-detects the VM's primary NIC and creates `rxe0` device
4. Verifies with `ibv_devices` and loopback `rping`

Set `PROJECT_DIR` env var if running from a non-standard location:
```bash
PROJECT_DIR=/path/to/rust-rdma-io ./run_tests.sh
```
