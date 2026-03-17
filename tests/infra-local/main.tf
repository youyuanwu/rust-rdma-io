# Local RDMA Testing Infrastructure
# Creates 2 VMs with single NIC each for RDMA testing using QEMU/KVM + rxe

locals {
  ssh_public_key = var.ssh_public_key != "" ? var.ssh_public_key : file(pathexpand(var.ssh_key_path))
}

# =============================================================================
# Network
# =============================================================================

# Ensure default network exists (NAT + DHCP for management and RDMA)
resource "null_resource" "default_network" {
  triggers = {
    network_name = "default"
  }

  provisioner "local-exec" {
    command = <<-EOT
      set -e
      export LIBVIRT_DEFAULT_URI="qemu:///system"
      NETWORK_NAME="default"

      if virsh net-info $NETWORK_NAME 2>/dev/null | grep -q "Active:.*yes"; then
        echo "Network $NETWORK_NAME already exists and is active"
        exit 0
      fi

      if virsh net-info $NETWORK_NAME 2>/dev/null; then
        echo "Network $NETWORK_NAME exists but not active, starting..."
        virsh net-start $NETWORK_NAME || true
        virsh net-autostart $NETWORK_NAME || true
        exit 0
      fi

      cat > /tmp/$NETWORK_NAME.xml <<EOF
<network>
  <name>$NETWORK_NAME</name>
  <forward mode='nat'/>
  <bridge name='virbr0' stp='on' delay='0'/>
  <ip address='192.168.122.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='192.168.122.2' end='192.168.122.254'/>
    </dhcp>
  </ip>
</network>
EOF

      echo "Creating network $NETWORK_NAME..."
      virsh net-define /tmp/$NETWORK_NAME.xml
      virsh net-start $NETWORK_NAME
      virsh net-autostart $NETWORK_NAME
      rm -f /tmp/$NETWORK_NAME.xml

      sleep 1
      virsh net-info $NETWORK_NAME
      echo "Network $NETWORK_NAME created successfully"
    EOT
  }

  provisioner "local-exec" {
    when    = destroy
    command = <<-EOT
      export LIBVIRT_DEFAULT_URI="qemu:///system"
      virsh net-destroy default 2>/dev/null || true
      virsh net-undefine default 2>/dev/null || true
    EOT
  }
}

# =============================================================================
# Base Image
# =============================================================================

resource "libvirt_volume" "base_image" {
  name = "rdma-base-ubuntu-24.04.qcow2"
  pool = var.storage_pool

  create = {
    content = {
      url = var.base_image_url
    }
  }
}

# =============================================================================
# VM1
# =============================================================================

resource "libvirt_volume" "vm1_disk" {
  name     = "rdma-vm1-disk.qcow2"
  pool     = var.storage_pool
  capacity = var.disk_size

  target = {
    format = {
      type = "qcow2"
    }
  }

  backing_store = {
    path = libvirt_volume.base_image.path
    format = {
      type = "qcow2"
    }
  }
}

resource "libvirt_cloudinit_disk" "vm1_cloudinit" {
  name = "rdma-vm1-cloudinit"

  meta_data = yamlencode({
    instance-id    = "rdma-vm1-${formatdate("YYYYMMDDhhmmss", timestamp())}"
    local-hostname = "rdma-vm1"
  })

  user_data = <<-EOF
    #cloud-config
    hostname: rdma-vm1
    fqdn: rdma-vm1.local
    manage_etc_hosts: true

    users:
      - name: ${var.ssh_user}
        sudo: ALL=(ALL) NOPASSWD:ALL
        shell: /bin/bash
        ssh_authorized_keys:
          - ${local.ssh_public_key}

    package_update: true
    package_upgrade: false

    packages:
      - rdma-core
      - libibverbs1
      - librdmacm1t64
      - rdmacm-utils
      - ibverbs-utils
      - iproute2
  EOF

  network_config = yamlencode({
    version = 2
    ethernets = {
      eth0 = {
        match = {
          name = "en*"
        }
        dhcp4 = true
      }
    }
  })
}

resource "libvirt_volume" "vm1_cloudinit" {
  name = "rdma-vm1-cloudinit.iso"
  pool = var.storage_pool

  create = {
    content = {
      url = libvirt_cloudinit_disk.vm1_cloudinit.path
    }
  }

  lifecycle {
    replace_triggered_by = [libvirt_cloudinit_disk.vm1_cloudinit]
  }
}

resource "libvirt_domain" "vm1" {
  name        = "rdma-vm1"
  memory      = var.vm_memory_mb
  memory_unit = "MiB"
  vcpu        = var.vm_vcpus
  type        = var.use_kvm ? "kvm" : "qemu"
  running     = true
  autostart   = true

  depends_on = [null_resource.default_network]

  os = {
    type      = "hvm"
    type_arch = "x86_64"
    boot_devices = [
      { dev = "hd" }
    ]
  }

  cpu = {
    mode  = var.use_kvm ? "host-passthrough" : "custom"
    model = var.use_kvm ? null : "Nehalem"
  }

  devices = {
    disks = [
      {
        driver = {
          type = "qcow2"
        }
        source = {
          volume = {
            pool   = var.storage_pool
            volume = libvirt_volume.vm1_disk.name
          }
        }
        target = {
          dev = "vda"
          bus = "virtio"
        }
      },
      {
        device = "cdrom"
        source = {
          volume = {
            pool   = var.storage_pool
            volume = libvirt_volume.vm1_cloudinit.name
          }
        }
        target = {
          dev = "sda"
          bus = "sata"
        }
        readonly = true
      }
    ]

    interfaces = [
      {
        model = {
          type = "virtio"
        }
        source = {
          network = {
            network = "default"
          }
        }
      }
    ]

    consoles = [
      {
        target = {
          type = "serial"
          port = 0
        }
      }
    ]

    graphics = [
      {
        vnc = {
          auto_port = true
        }
      }
    ]

    channels = [
      {
        target = {
          virt_io = {
            name = "org.qemu.guest_agent.0"
          }
        }
      }
    ]
  }
}

# =============================================================================
# VM2
# =============================================================================

resource "libvirt_volume" "vm2_disk" {
  name     = "rdma-vm2-disk.qcow2"
  pool     = var.storage_pool
  capacity = var.disk_size

  target = {
    format = {
      type = "qcow2"
    }
  }

  backing_store = {
    path = libvirt_volume.base_image.path
    format = {
      type = "qcow2"
    }
  }
}

resource "libvirt_cloudinit_disk" "vm2_cloudinit" {
  name = "rdma-vm2-cloudinit"

  meta_data = yamlencode({
    instance-id    = "rdma-vm2-${formatdate("YYYYMMDDhhmmss", timestamp())}"
    local-hostname = "rdma-vm2"
  })

  user_data = <<-EOF
    #cloud-config
    hostname: rdma-vm2
    fqdn: rdma-vm2.local
    manage_etc_hosts: true

    users:
      - name: ${var.ssh_user}
        sudo: ALL=(ALL) NOPASSWD:ALL
        shell: /bin/bash
        ssh_authorized_keys:
          - ${local.ssh_public_key}

    package_update: true
    package_upgrade: false

    packages:
      - rdma-core
      - libibverbs1
      - librdmacm1t64
      - rdmacm-utils
      - ibverbs-utils
      - iproute2
  EOF

  network_config = yamlencode({
    version = 2
    ethernets = {
      eth0 = {
        match = {
          name = "en*"
        }
        dhcp4 = true
      }
    }
  })
}

resource "libvirt_volume" "vm2_cloudinit" {
  name = "rdma-vm2-cloudinit.iso"
  pool = var.storage_pool

  create = {
    content = {
      url = libvirt_cloudinit_disk.vm2_cloudinit.path
    }
  }

  lifecycle {
    replace_triggered_by = [libvirt_cloudinit_disk.vm2_cloudinit]
  }
}

resource "libvirt_domain" "vm2" {
  name        = "rdma-vm2"
  memory      = var.vm_memory_mb
  memory_unit = "MiB"
  vcpu        = var.vm_vcpus
  type        = var.use_kvm ? "kvm" : "qemu"
  running     = true
  autostart   = true

  depends_on = [null_resource.default_network]

  os = {
    type      = "hvm"
    type_arch = "x86_64"
    boot_devices = [
      { dev = "hd" }
    ]
  }

  cpu = {
    mode  = var.use_kvm ? "host-passthrough" : "custom"
    model = var.use_kvm ? null : "Nehalem"
  }

  devices = {
    disks = [
      {
        driver = {
          type = "qcow2"
        }
        source = {
          volume = {
            pool   = var.storage_pool
            volume = libvirt_volume.vm2_disk.name
          }
        }
        target = {
          dev = "vda"
          bus = "virtio"
        }
      },
      {
        device = "cdrom"
        source = {
          volume = {
            pool   = var.storage_pool
            volume = libvirt_volume.vm2_cloudinit.name
          }
        }
        target = {
          dev = "sda"
          bus = "sata"
        }
        readonly = true
      }
    ]

    interfaces = [
      {
        model = {
          type = "virtio"
        }
        source = {
          network = {
            network = "default"
          }
        }
      }
    ]

    consoles = [
      {
        target = {
          type = "serial"
          port = 0
        }
      }
    ]

    graphics = [
      {
        vnc = {
          auto_port = true
        }
      }
    ]

    channels = [
      {
        target = {
          virt_io = {
            name = "org.qemu.guest_agent.0"
          }
        }
      }
    ]
  }
}
