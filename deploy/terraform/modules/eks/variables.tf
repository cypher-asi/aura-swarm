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

variable "private_subnet_ids" {
  description = "List of private subnet IDs for EKS"
  type        = list(string)
}

variable "agent_subnet_ids" {
  description = "List of agent subnet IDs for node groups"
  type        = list(string)
}

variable "node_instance_type" {
  description = "EC2 instance type for worker nodes"
  type        = string
  default     = "m5.2xlarge"
}

variable "node_desired_count" {
  description = "Desired number of worker nodes"
  type        = number
  default     = 2
}

variable "node_min_count" {
  description = "Minimum number of worker nodes"
  type        = number
  default     = 1
}

variable "node_max_count" {
  description = "Maximum number of worker nodes"
  type        = number
  default     = 5
}

variable "node_disk_size" {
  description = "Disk size in GB for worker nodes"
  type        = number
  default     = 100
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}
