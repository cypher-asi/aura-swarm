variable "resource_prefix" {
  description = "Prefix for resource names"
  type        = string
}

variable "vpc_id" {
  description = "ID of the VPC"
  type        = string
}

variable "subnet_ids" {
  description = "List of subnet IDs for EFS mount targets"
  type        = list(string)
}

variable "agent_subnet_cidrs" {
  description = "CIDR blocks of agent subnets (for security group)"
  type        = list(string)
}

variable "encrypted" {
  description = "Enable encryption for EFS"
  type        = bool
  default     = true
}

variable "throughput_mode" {
  description = "EFS throughput mode (bursting or provisioned)"
  type        = string
  default     = "bursting"
}

variable "performance_mode" {
  description = "EFS performance mode (generalPurpose or maxIO)"
  type        = string
  default     = "generalPurpose"
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}
