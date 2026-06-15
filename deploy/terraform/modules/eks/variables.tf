variable "resource_prefix" {
  description = "Prefix for resource names"
  type        = string
}

variable "eks_version" {
  description = "Kubernetes version for EKS cluster"
  type        = string
}

variable "vpc_id" {
  description = "ID of the VPC"
  type        = string
}

variable "vpc_cidr" {
  description = "CIDR block of the VPC (used to scope Peer Pods / CAA worker <-> pod-VM ingress rules)"
  type        = string
}

variable "private_subnet_ids" {
  description = "List of private subnet IDs for EKS"
  type        = list(string)
}

variable "agent_subnet_ids" {
  description = "List of agent subnet IDs for node groups"
  type        = list(string)
}

variable "node_instance_type" {
  description = "EC2 instance type for system worker nodes (gateway/scheduler/CSI etc.; agents run on the confidential pool)"
  type        = string
  default     = "m5.2xlarge"
}

variable "node_desired_count" {
  description = "Desired number of system worker nodes"
  type        = number
  default     = 2
}

variable "node_min_count" {
  description = "Minimum number of system worker nodes"
  type        = number
  default     = 1
}

variable "node_max_count" {
  description = "Maximum number of system worker nodes (shrunk in R3: the legacy microVM agent capacity is gone)"
  type        = number
  default     = 3
}

variable "node_disk_size" {
  description = "Disk size in GB for system worker nodes"
  type        = number
  default     = 100
}

#------------------------------------------------------------------------------
# Dedicated TEE worker pool (aura-swarm-tee-hosts): a second managed node group
# that hosts NEW confidential (kata-remote) agents on a clean pool with its own
# kata.peerpods.io/vm headroom and image cache, isolated from the shared pool.
#------------------------------------------------------------------------------
variable "tee_node_group_name" {
  description = "Name of the dedicated TEE (peer-pods) worker node group"
  type        = string
  default     = "aura-swarm-tee-hosts"
}

variable "tee_node_instance_type" {
  description = "EC2 instance type for the dedicated TEE worker nodes"
  type        = string
  default     = "m5.2xlarge"
}

variable "tee_node_desired_count" {
  description = "Desired number of TEE worker nodes"
  type        = number
  default     = 2
}

variable "tee_node_min_count" {
  description = "Minimum number of TEE worker nodes"
  type        = number
  default     = 1
}

variable "tee_node_max_count" {
  description = "Maximum number of TEE worker nodes"
  type        = number
  default     = 5
}

# Pin the AL2023 EKS-optimized AMI to a release that still ships containerd 1.7.x.
#
# WHY: kata-remote (Peer Pods) guest image pull needs containerd to pass per-layer
# snapshot annotations (cri.image-ref / cri.layer-digest / ...) to the nydus
# "guest pull" snapshotter so the WORKLOAD image is pulled INSIDE the guest VM.
# containerd 2.x on AL2023 (the default for EKS 1.31 AMIs from release v20251108
# onward, e.g. containerd 2.2.1) does NOT attach those labels in the
# CreateContainer unpack path, so nydus gets empty labels, falls back to
# "dummy-image-reference", and the host then tries to unpack the image itself and
# fails with `content digest ...: not found` (CreateContainerError). The upstream
# fix is containerd PR #12835, which is not yet in any released 2.x AMI.
#
# v20251103 is the last EKS 1.31 AL2023 release on containerd 1.7.27 (k8s 1.31.13),
# where guest pull works (cri_handler="cc" path). Empty string = use the latest
# EKS-optimized AMI (do this only once an AMI ships a containerd with PR #12835).
#
# NOTE: this string is Kubernetes-version specific. If eks_version changes, update
# this to the matching last-containerd-1.7.x release for that minor (see
# awslabs/amazon-eks-ami issue #2470 for the per-version cutover dates).
variable "node_ami_release_version" {
  description = "EKS AL2023 node AMI release_version to pin (containerd 1.7.x for kata-remote guest pull); empty = latest"
  type        = string
  default     = "1.31.13-20251103"
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}
