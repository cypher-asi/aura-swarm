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

variable "availability_zones" {
  description = "List of availability zones"
  type        = list(string)
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}
