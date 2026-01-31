output "cluster_name" {
  description = "Name of the EKS cluster"
  value       = aws_eks_cluster.main.name
}

output "cluster_arn" {
  description = "ARN of the EKS cluster"
  value       = aws_eks_cluster.main.arn
}

output "cluster_endpoint" {
  description = "Endpoint URL for the EKS cluster API server"
  value       = aws_eks_cluster.main.endpoint
}

output "cluster_certificate_authority_data" {
  description = "Base64 encoded certificate data for cluster"
  value       = aws_eks_cluster.main.certificate_authority[0].data
}

output "cluster_security_group_id" {
  description = "Security group ID attached to the EKS cluster"
  value       = aws_security_group.cluster.id
}

output "cluster_version" {
  description = "Kubernetes version of the cluster"
  value       = aws_eks_cluster.main.version
}

output "oidc_provider_arn" {
  description = "ARN of the OIDC provider for IRSA"
  value       = aws_iam_openid_connect_provider.cluster.arn
}

output "oidc_provider_url" {
  description = "URL of the OIDC provider (without https://)"
  value       = replace(aws_eks_cluster.main.identity[0].oidc[0].issuer, "https://", "")
}

output "node_group_role_arn" {
  description = "ARN of the EKS node group IAM role"
  value       = aws_iam_role.node_group.arn
}

output "node_group_role_name" {
  description = "Name of the EKS node group IAM role"
  value       = aws_iam_role.node_group.name
}

output "node_security_group_id" {
  description = "Security group ID attached to worker nodes"
  value       = aws_security_group.node.id
}

output "cluster_role_arn" {
  description = "ARN of the EKS cluster IAM role"
  value       = aws_iam_role.cluster.arn
}
