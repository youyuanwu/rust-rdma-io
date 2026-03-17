output "vm1_name" {
  description = "Name of VM1"
  value       = libvirt_domain.vm1.name
}

output "vm2_name" {
  description = "Name of VM2"
  value       = libvirt_domain.vm2.name
}

output "ssh_user" {
  description = "SSH username for VMs"
  value       = var.ssh_user
}

output "management_network" {
  description = "Management network name (DHCP)"
  value       = "default"
}

output "get_vm_ips" {
  description = "Command to get VM IPs"
  value       = "virsh net-dhcp-leases default"
}
