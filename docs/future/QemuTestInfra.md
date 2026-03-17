# QEMU/KVM Local Test Infrastructure for RDMA

Local VM-based testing framework using Terraform and QEMU/KVM,
modeled after [rust-dpdk-net/tests/infra-local](https://github.com/youyuanwu/rust-dpdk-net/tree/main/tests/infra-local).

**Status:** Phase 1 implemented and tested. See `tests/infra-local/` and `tests/e2e/`.

## Motivation

| Limitation of current CI | Impact |
|--------------------------|--------|
| GitHub runner kernel is fixed | Cannot test newer kernels with RDMA fixes |
| Single-machine only | Cannot test two-node RDMA scenarios |
| Flaky siw/rxe cleanup | Kernel module state leaks between runs |
| No persistent environment | Every CI run rebuilds from scratch |

## Design Principles

- **No building in the VM.** Pre-built binaries copied via Ansible (future).
- **rxe only.** Soft-RoCE provides the full verbs surface including MW Type 2.
- **Always 2 VMs.** Server (VM1) + client (VM2) for cross-node testing.
- **Single NIC per VM.** rxe shares the NIC with SSH — no dedicated RDMA network needed (unlike DPDK which requires exclusive NIC access).
- **Ansible-driven tests.** Playbooks handle rxe setup, verification, and test execution.
- **Minimal VM image.** Cloud-init installs `rdma-core` runtime only. Ansible installs `linux-modules-extra` and loads kernel modules.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                          Host Machine                               │
│                                                                     │
│   ansible-playbook → sets up rxe + runs tests                       │
│                                                                     │
│   ┌──────────────┐                        ┌──────────────┐         │
│   │   rdma-vm1   │                        │   rdma-vm2   │         │
│   │   (server)   │                        │   (client)   │         │
│   │              │                        │              │         │
│   │  enp0s3      │                        │  enp0s3      │         │
│   │  DHCP        │                        │  DHCP        │         │
│   │  rxe0        │                        │  rxe0        │         │
│   │  [SSH+RDMA]  │                        │  [SSH+RDMA]  │         │
│   └──────┬───────┘                        └──────┬───────┘         │
│          │           default network             │                  │
│          └──────────── (NAT + DHCP) ────────────┘                  │
│                192.168.122.0/24                                      │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## Directory Structure

```
tests/
├── infra-local/                    # Terraform VM provisioning
│   ├── main.tf                     # 2 VMs, default network, cloud-init
│   ├── variables.tf                # libvirt_uri, ssh_key, vm resources
│   ├── versions.tf                 # dmacvicar/libvirt ~> 0.9, hashicorp/null
│   ├── outputs.tf, terraform.tfvars, .gitignore
│   ├── README.md
│   └── scripts/
│       ├── start-vm.sh             # Prereq checks + terraform apply
│       ├── destroy-vm.sh           # terraform destroy + pool cleanup
│       └── ssh-vm.sh               # DHCP lease discovery + SSH
└── e2e/                            # Ansible test orchestration
    ├── ansible.cfg                 # SSH config, inventory path
    ├── inventory_local.py          # Dynamic inventory from virsh DHCP leases
    ├── run_tests.sh                # Entry point
    ├── README.md
    └── playbooks/
        ├── hello_world.yml         # Basic connectivity + OS info
        ├── test_connectivity.yml   # Ping between VMs
        ├── setup_rdma.yml          # Install modules-extra, load rxe, create device
        ├── rdma_ping_test.yml      # Cross-node rping (server VM1, client VM2)
        └── run_tests.yml           # Full suite: setup → connectivity → rping
```

## Quick Start

```bash
# Deploy 2 VMs
./tests/infra-local/scripts/start-vm.sh -y

# Run full E2E test suite
./tests/e2e/run_tests.sh

# Individual playbooks
./tests/e2e/run_tests.sh playbooks/hello_world.yml
./tests/e2e/run_tests.sh playbooks/setup_rdma.yml

# SSH into VMs
./tests/infra-local/scripts/ssh-vm.sh 1
./tests/infra-local/scripts/ssh-vm.sh 2

# Tear down
./tests/infra-local/scripts/destroy-vm.sh -y
```

## Key Implementation Details

### What `start-vm.sh` auto-configures

The script handles all prerequisites that Terraform and QEMU need:

1. **QEMU security** — sets `security_driver = "none"` in `/etc/libvirt/qemu.conf` (allows QEMU to access files outside `/var/lib/libvirt`)
2. **Home directory traversal** — `chmod o+x $HOME` (QEMU uid:64055 needs +x to traverse path)
3. **Storage pool** — creates `rdma-local` pool in `build/vm-images/` (project-local, not `/var/lib/libvirt/images`)
4. **Default network** — created via `null_resource` in Terraform (NAT + DHCP on 192.168.122.0/24)

### What `setup_rdma.yml` does (idempotent)

1. Installs `linux-modules-extra-$(uname -r)` via apt (Ubuntu cloud images ship minimal kernel)
2. Loads `ib_uverbs`, `rdma_cm`, `rdma_ucm` — skips if already loaded (`lsmod` check)
3. Loads `rdma_rxe` via `modprobe` — if fails, copies host-built `.ko` and uses `insmod`
4. Auto-detects primary NIC via `ip -o link show`
5. Creates `rxe0` device — skips if exists (`rdma link show | grep rxe0`)
6. Verifies with `ibv_devices` + loopback `rping`

### Ubuntu 24.04 package names

Ubuntu 24.04 renamed some packages (transition to 64-bit time_t):

| Expected | Actual (Noble) |
|----------|----------------|
| `librdmacm1` | `librdmacm1t64` |
| `librdmacm-utils` | `rdmacm-utils` |

### Inventory requires `qemu:///system`

User sessions default to `qemu:///session` (no networks). The inventory script
sets `LIBVIRT_DEFAULT_URI=qemu:///system` to see the VMs. User must be in the
`libvirt` group.

## Comparison: Current CI vs QEMU Infra

| Aspect | Current CI (bare metal) | QEMU Local Infra |
|--------|------------------------|------------------|
| Setup time | ~1 min (apt install) | ~2 min (VM boot + cloud-init) |
| Test speed | Native | KVM: ~1.05x, QEMU: ~5-10x |
| Kernel control | Runner's kernel only | Any Ubuntu version |
| Multi-node | ❌ Single machine | ✅ 2 VMs |
| State isolation | Shared runner | Fresh VM per run |
| Test orchestration | Shell scripts | Ansible playbooks |
| Reproducibility | Depends on runner | Fully deterministic |

## CI Integration (Future)

### GitHub Actions (QEMU, no KVM)

```yaml
qemu-rdma-test:
  runs-on: ubuntu-latest
  timeout-minutes: 60
  steps:
    - uses: actions/checkout@v6
    - name: Install QEMU, libvirt, Terraform, Ansible
      run: |
        sudo apt-get update
        sudo apt-get install -y qemu-kvm libvirt-daemon-system \
            libvirt-clients libvirt-dev virtinst qemu-utils ansible
        sudo systemctl start libvirtd
    - uses: hashicorp/setup-terraform@v3
    - run: ./tests/infra-local/scripts/start-vm.sh --no-kvm -y
    - run: ./tests/e2e/run_tests.sh
    - run: ./tests/infra-local/scripts/destroy-vm.sh -y
      if: always()
```

### Self-Hosted Runner (KVM)

```yaml
qemu-rdma-test:
  runs-on: self-hosted
  steps:
    - uses: actions/checkout@v6
    - run: ./tests/infra-local/scripts/start-vm.sh -y
    - run: ./tests/e2e/run_tests.sh
    - run: ./tests/infra-local/scripts/destroy-vm.sh -y
      if: always()
```

## Future Work

### Phase 2: Custom RDMA App Tests

- Build server/client example binaries
- `deploy_binaries.yml` playbook (copy pre-built binaries via Ansible)
- `rdma_echo_test.yml`, `tonic_grpc_test.yml`, `quinn_quic_test.yml`
- `ring_transport_test.yml`

### Phase 3: CI Integration

- GitHub Actions workflow (QEMU, weekly)
- Self-hosted runner workflow (KVM, per-push)

### Phase 4: Advanced

- Custom kernel testing
- Performance benchmarking (`ib_write_bw` between VMs)
- Pre-built VM images (skip cloud-init on subsequent runs)

## Troubleshooting

### Bridge netfilter blocks rxe traffic

If cross-VM rxe traffic fails, bridge netfilter may filter RoCEv2 UDP (port 4791):

```bash
sudo sysctl net.bridge.bridge-nf-call-iptables=0
```

### Permission denied on VM disk

QEMU (uid:64055) can't traverse the path to `build/vm-images/`. Fix:

```bash
chmod o+x $HOME     # allow traversal
```

`start-vm.sh` does this automatically.

### `virsh net-dhcp-leases default` shows nothing

Use `qemu:///system` connection:

```bash
LIBVIRT_DEFAULT_URI=qemu:///system virsh net-dhcp-leases default
```

Or ensure your user is in the `libvirt` group and the inventory script handles this.

### `rdma_rxe` module not found

Ubuntu cloud images ship minimal kernels. `setup_rdma.yml` installs
`linux-modules-extra` automatically. If that doesn't include `rdma_rxe`,
build it on the host (`cmake --build build --target rxe`) and the playbook
will copy and `insmod` it.
