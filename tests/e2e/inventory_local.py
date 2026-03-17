#!/usr/bin/env python3
"""
Dynamic Ansible inventory for local QEMU/KVM RDMA VMs.
Gets VM IPs from libvirt DHCP leases.
"""

import json
import os
import subprocess
import sys
import re


def get_dhcp_leases():
    """Get DHCP leases from libvirt default network."""
    env = dict(os.environ, LIBVIRT_DEFAULT_URI="qemu:///system")
    try:
        result = subprocess.run(
            ["virsh", "net-dhcp-leases", "default"],
            capture_output=True, text=True, check=True, env=env
        )
        return result.stdout
    except (subprocess.CalledProcessError, FileNotFoundError) as e:
        sys.stderr.write(f"Error getting DHCP leases: {e}\n")
        return ""


def parse_leases(output):
    """Parse virsh net-dhcp-leases output."""
    vms = {}
    for line in output.splitlines():
        for name in ["rdma-vm1", "rdma-vm2"]:
            if name in line:
                match = re.search(r'(\d+\.\d+\.\d+\.\d+)', line)
                if match:
                    vms[name] = match.group(1)
    return vms


def load_inventory():
    """Build inventory from local VMs."""
    vms = parse_leases(get_dhcp_leases())

    inventory = {
        "_meta": {"hostvars": {}},
        "all": {"children": ["vms"]},
        "vms": {"hosts": []},
    }

    for i, (name, ip) in enumerate(sorted(vms.items()), 1):
        host = f"vm{i}"
        inventory["vms"]["hosts"].append(host)
        inventory["_meta"]["hostvars"][host] = {
            "ansible_host": ip,
            "vm_name": name,
            "is_local": True,
        }

    return inventory


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--list":
        inventory = load_inventory()
        print(json.dumps(inventory, indent=2))
    elif len(sys.argv) == 3 and sys.argv[1] == "--host":
        print(json.dumps({}))
    else:
        sys.stderr.write("Usage: inventory_local.py --list | --host <hostname>\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
