variable "libvirt_uri" {
  description = "Libvirt connection URI"
  type        = string
  default     = "qemu:///system"
}

variable "ssh_public_key" {
  description = "SSH public key for VM access (if empty, reads from ssh_key_path)"
  type        = string
  default     = ""
}

variable "ssh_key_path" {
  description = "Path to SSH public key file"
  type        = string
  default     = "~/.ssh/id_rsa.pub"
}

variable "ssh_user" {
  description = "SSH username for VMs"
  type        = string
  default     = "azureuser"
}

variable "storage_pool" {
  description = "Libvirt storage pool name"
  type        = string
  default     = "rdma-local"
}

variable "base_image_url" {
  description = "URL to Ubuntu cloud image"
  type        = string
  default     = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
}

variable "vm_memory_mb" {
  description = "Memory per VM in MB"
  type        = number
  default     = 2048
}

variable "vm_vcpus" {
  description = "vCPUs per VM"
  type        = number
  default     = 2
}

variable "disk_size" {
  description = "OS disk size in bytes (default 6GB)"
  type        = number
  default     = 6442450944 # 6 GB
}

variable "use_kvm" {
  description = "Use KVM hardware acceleration (false for software emulation)"
  type        = bool
  default     = true
}
