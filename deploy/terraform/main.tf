#------------------------------------------------------------------------------
# Aura Swarm Infrastructure - Root Module
#------------------------------------------------------------------------------

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = merge(
      {
        Project     = var.project_name
        Environment = var.environment
        ManagedBy   = "terraform"
      },
      var.tags
    )
  }
}

# Fetch available AZs if not specified
data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  resource_prefix = "${var.project_name}-${var.environment}"
  
  # Use provided AZs or auto-detect (limit to 2 for cost savings in dev)
  availability_zones = length(var.availability_zones) > 0 ? var.availability_zones : slice(data.aws_availability_zones.available.names, 0, 2)
  
  common_tags = {
    Project     = var.project_name
    Environment = var.environment
  }
}

#------------------------------------------------------------------------------
# Network Module - VPC, Subnets, NAT Gateway
#------------------------------------------------------------------------------

module "network" {
  source = "./modules/network"
  count  = var.enable_network ? 1 : 0

  resource_prefix     = local.resource_prefix
  vpc_cidr            = var.vpc_cidr
  public_subnet_cidr  = var.public_subnet_cidr
  private_subnet_cidr = var.private_subnet_cidr
  agent_subnet_cidr   = var.agent_subnet_cidr
  storage_subnet_cidr = var.storage_subnet_cidr
  availability_zones  = local.availability_zones
  tags                = local.common_tags
}

#------------------------------------------------------------------------------
# Storage Module - EFS Filesystem
#------------------------------------------------------------------------------

module "storage" {
  source = "./modules/storage"
  count  = var.enable_storage && var.enable_network ? 1 : 0

  resource_prefix    = local.resource_prefix
  vpc_id             = module.network[0].vpc_id
  subnet_ids         = module.network[0].storage_subnet_ids
  agent_subnet_cidrs = module.network[0].agent_subnet_cidrs
  encrypted          = var.efs_encrypted
  throughput_mode    = var.efs_throughput_mode
  performance_mode   = var.efs_performance_mode
  tags               = local.common_tags
}

#------------------------------------------------------------------------------
# EKS Module - Kubernetes Cluster
#------------------------------------------------------------------------------

module "eks" {
  source = "./modules/eks"
  count  = var.enable_eks && var.enable_network ? 1 : 0

  resource_prefix      = local.resource_prefix
  eks_version          = var.eks_version
  vpc_id               = module.network[0].vpc_id
  private_subnet_ids   = module.network[0].private_subnet_ids
  agent_subnet_ids     = module.network[0].agent_subnet_ids
  node_instance_type   = var.node_instance_type
  node_desired_count   = var.node_desired_count
  node_min_count       = var.node_min_count
  node_max_count       = var.node_max_count
  node_disk_size       = var.node_disk_size
  tags                 = local.common_tags
}

#------------------------------------------------------------------------------
# ECR Module - Container Registries
#------------------------------------------------------------------------------

module "ecr" {
  source = "./modules/ecr"
  count  = var.enable_ecr ? 1 : 0

  resource_prefix          = local.resource_prefix
  repositories             = var.ecr_repositories
  image_retention_count    = var.ecr_image_retention_count
  tags                     = local.common_tags
}
