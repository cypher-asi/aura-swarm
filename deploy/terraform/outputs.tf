#------------------------------------------------------------------------------
# Network Outputs
#------------------------------------------------------------------------------

output "vpc_id" {
  description = "ID of the VPC"
  value       = var.enable_network ? module.network[0].vpc_id : null
}

output "vpc_cidr" {
  description = "CIDR block of the VPC"
  value       = var.enable_network ? module.network[0].vpc_cidr : null
}

output "public_subnet_ids" {
  description = "IDs of public subnets"
  value       = var.enable_network ? module.network[0].public_subnet_ids : []
}

output "private_subnet_ids" {
  description = "IDs of private subnets"
  value       = var.enable_network ? module.network[0].private_subnet_ids : []
}

output "agent_subnet_ids" {
  description = "IDs of agent subnets"
  value       = var.enable_network ? module.network[0].agent_subnet_ids : []
}

output "storage_subnet_ids" {
  description = "IDs of storage subnets"
  value       = var.enable_network ? module.network[0].storage_subnet_ids : []
}

output "nat_gateway_ip" {
  description = "Public IP of NAT Gateway"
  value       = var.enable_network ? module.network[0].nat_gateway_ip : null
}

#------------------------------------------------------------------------------
# Storage Outputs
#------------------------------------------------------------------------------

output "efs_filesystem_id" {
  description = "ID of the EFS filesystem"
  value       = var.enable_storage && var.enable_network ? module.storage[0].filesystem_id : null
}

output "efs_filesystem_arn" {
  description = "ARN of the EFS filesystem"
  value       = var.enable_storage && var.enable_network ? module.storage[0].filesystem_arn : null
}

output "efs_dns_name" {
  description = "DNS name for the EFS filesystem"
  value       = var.enable_storage && var.enable_network ? module.storage[0].dns_name : null
}

#------------------------------------------------------------------------------
# EKS Outputs
#------------------------------------------------------------------------------

output "eks_cluster_name" {
  description = "Name of the EKS cluster"
  value       = var.enable_eks && var.enable_network ? module.eks[0].cluster_name : null
}

output "eks_cluster_endpoint" {
  description = "Endpoint URL for the EKS cluster API server"
  value       = var.enable_eks && var.enable_network ? module.eks[0].cluster_endpoint : null
}

output "eks_cluster_arn" {
  description = "ARN of the EKS cluster"
  value       = var.enable_eks && var.enable_network ? module.eks[0].cluster_arn : null
}

output "eks_cluster_security_group_id" {
  description = "Security group ID attached to the EKS cluster"
  value       = var.enable_eks && var.enable_network ? module.eks[0].cluster_security_group_id : null
}

output "eks_oidc_provider_arn" {
  description = "ARN of the OIDC provider for IRSA"
  value       = var.enable_eks && var.enable_network ? module.eks[0].oidc_provider_arn : null
}

output "eks_oidc_provider_url" {
  description = "URL of the OIDC provider"
  value       = var.enable_eks && var.enable_network ? module.eks[0].oidc_provider_url : null
}

output "eks_node_group_role_arn" {
  description = "ARN of the EKS node group IAM role"
  value       = var.enable_eks && var.enable_network ? module.eks[0].node_group_role_arn : null
}

output "eks_update_kubeconfig_command" {
  description = "Command to update kubeconfig"
  value       = var.enable_eks && var.enable_network ? "aws eks update-kubeconfig --region ${var.aws_region} --name ${module.eks[0].cluster_name}" : null
}

#------------------------------------------------------------------------------
# ECR Outputs
#------------------------------------------------------------------------------

output "ecr_repository_urls" {
  description = "Map of ECR repository names to URLs"
  value       = var.enable_ecr ? module.ecr[0].repository_urls : {}
}

output "ecr_repository_arns" {
  description = "Map of ECR repository names to ARNs"
  value       = var.enable_ecr ? module.ecr[0].repository_arns : {}
}

#------------------------------------------------------------------------------
# Summary Output
#------------------------------------------------------------------------------

output "deployment_summary" {
  description = "Summary of deployed resources"
  value = {
    project         = var.project_name
    environment     = var.environment
    region          = var.aws_region
    vpc_id          = var.enable_network ? module.network[0].vpc_id : null
    eks_cluster     = var.enable_eks && var.enable_network ? module.eks[0].cluster_name : null
    efs_id          = var.enable_storage && var.enable_network ? module.storage[0].filesystem_id : null
    ecr_repos       = var.enable_ecr ? keys(module.ecr[0].repository_urls) : []
  }
}
