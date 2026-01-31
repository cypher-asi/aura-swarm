#------------------------------------------------------------------------------
# General Configuration
#------------------------------------------------------------------------------

variable "aws_region" {
  description = "AWS region for all resources"
  type        = string
  default     = "us-east-2"
}

variable "project_name" {
  description = "Project name used for resource naming"
  type        = string
  default     = "aura-swarm"
}

variable "environment" {
  description = "Environment (dev, staging, prod)"
  type        = string
  default     = "dev"
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}

#------------------------------------------------------------------------------
# Network Configuration
#------------------------------------------------------------------------------

variable "vpc_cidr" {
  description = "CIDR block for VPC"
  type        = string
  default     = "10.0.0.0/16"
}

variable "public_subnet_cidr" {
  description = "CIDR block for public subnet (load balancers)"
  type        = string
  default     = "10.0.1.0/24"
}

variable "private_subnet_cidr" {
  description = "CIDR block for private subnet (platform services)"
  type        = string
  default     = "10.0.2.0/24"
}

variable "agent_subnet_cidr" {
  description = "CIDR block for agent subnet (microVM pods)"
  type        = string
  default     = "10.0.3.0/24"
}

variable "storage_subnet_cidr" {
  description = "CIDR block for storage subnet (EFS mount targets)"
  type        = string
  default     = "10.0.4.0/24"
}

variable "availability_zones" {
  description = "List of availability zones (leave empty to auto-detect)"
  type        = list(string)
  default     = []
}

#------------------------------------------------------------------------------
# EKS Configuration
#------------------------------------------------------------------------------

variable "eks_version" {
  description = "Kubernetes version for EKS cluster"
  type        = string
  default     = "1.31"
}

variable "node_instance_type" {
  description = "EC2 instance type for EKS worker nodes"
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

#------------------------------------------------------------------------------
# ECR Configuration
#------------------------------------------------------------------------------

variable "ecr_repositories" {
  description = "List of ECR repository names to create"
  type        = list(string)
  default = [
    "gateway",
    "control",
    "scheduler"
  ]
}

variable "ecr_image_retention_count" {
  description = "Number of images to retain in ECR"
  type        = number
  default     = 30
}

#------------------------------------------------------------------------------
# Storage Configuration
#------------------------------------------------------------------------------

variable "efs_encrypted" {
  description = "Enable encryption for EFS"
  type        = bool
  default     = true
}

variable "efs_throughput_mode" {
  description = "EFS throughput mode (bursting or provisioned)"
  type        = string
  default     = "bursting"
}

variable "efs_performance_mode" {
  description = "EFS performance mode (generalPurpose or maxIO)"
  type        = string
  default     = "generalPurpose"
}

#------------------------------------------------------------------------------
# Feature Flags
#------------------------------------------------------------------------------

variable "enable_network" {
  description = "Enable network module (VPC, subnets)"
  type        = bool
  default     = true
}

variable "enable_storage" {
  description = "Enable storage module (EFS)"
  type        = bool
  default     = true
}

variable "enable_eks" {
  description = "Enable EKS module"
  type        = bool
  default     = true
}

variable "enable_ecr" {
  description = "Enable ECR module"
  type        = bool
  default     = true
}
