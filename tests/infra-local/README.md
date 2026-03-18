# Local RDMA Testing Infrastructure

Terraform configuration for provisioning QEMU/KVM virtual machines for RDMA testing with Soft-RoCE (rxe).

## Features

- **2 Ubuntu 24.04 VMs** with single NIC each
- **Soft-RoCE (rxe)** kernel module for RDMA (set up via Ansible)
- **Cloud-init** for automatic SSH key + rdma-core package installation
- **KVM acceleration** with fallback to software emulation

## Prerequisites

1. **Install QEMU/KVM and libvirt:**
   ```bash
   sudo apt install -y qemu-kvm libvirt-daemon-system libvirt-clients libvirt-dev virtinst qemu-utils
   ```

2. **Add user to libvirt group:**
   ```bash
   sudo usermod -aG libvirt $USER
   newgrp libvirt
   ```

3. **Install Terraform:**
   ```bash
   wget https://releases.hashicorp.com/terraform/1.14.4/terraform_1.14.4_linux_amd64.zip
   unzip terraform_1.14.4_linux_amd64.zip
   sudo mv terraform /usr/local/bin/
   ```

4. **SSH key** at `~/.ssh/id_rsa.pub`

5. **Install Ansible:**
   ```bash
   sudo apt install -y ansible
   ```

## Quick Start

```bash
# Build rxe kernel module (if not shipped with VM kernel)
cmake -B build -DBUILD_RXE=ON
cmake --build build --target rxe

# Start VMs
./tests/infra-local/scripts/start-vm.sh -y

# Start VMs without KVM (software emulation)
./tests/infra-local/scripts/start-vm.sh --no-kvm -y

# Run E2E tests via Ansible
./tests/e2e/run_tests.sh

# Destroy VMs
./tests/infra-local/scripts/destroy-vm.sh -y
```

## Connecting to VMs

```bash
# SSH to VMs (auto-discovers IP, waits for SSH ready)
./tests/infra-local/scripts/ssh-vm.sh 1   # VM1
./tests/infra-local/scripts/ssh-vm.sh 2   # VM2

# Wait for cloud-init to finish (CI use)
./tests/infra-local/scripts/ssh-vm.sh 1 --wait=600

# Or get IPs manually
virsh net-dhcp-leases default
```

## Configuration

Edit `tests/infra-local/terraform.tfvars` to customize:

```hcl
vm_memory_mb = 2048   # RAM per VM (default)
vm_vcpus     = 2      # CPUs per VM (default)
```

For benchmarking, use more resources:
```bash
TF_VAR_vm_memory_mb=4096 TF_VAR_vm_vcpus=4 ./tests/infra-local/scripts/start-vm.sh -y
```

## Design Documents

- [QEMU Test Infrastructure](../../docs/future/QemuTestInfra.md)
- [RDMA Benchmark Server & Client](../../docs/future/RdmaBenchmark.md)
