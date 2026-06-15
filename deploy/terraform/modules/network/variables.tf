variable "resource_prefix" {
  description = "Prefix for resource names"
  type        = string
}

variable "vpc_cidr" {
  description = "CIDR block for VPC"
  type        = string
}

variable "public_subnet_cidr" {
  description = "CIDR block for public subnets"
  type        = string
}

variable "private_subnet_cidr" {
  description = "CIDR block for private subnets"
  type        = string
}

variable "agent_subnet_cidr" {
  description = "CIDR block for agent subnets"
  type        = string
}

variable "storage_subnet_cidr" {
  description = "CIDR block for storage subnets"
  type        = string
}

variable "podvm_subnet_cidr" {
  description = "CIDR block for the dedicated Peer Pods pod-VM subnets (NOT used by the VPC CNI for pod IPs, so pod VMs never run out of addresses). Large on purpose."
  type        = string
  default     = "10.0.8.0/21"
}

variable "availability_zones" {
  description = "List of availability zones"
  type        = list(string)
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}
