output "filesystem_id" {
  description = "ID of the EFS filesystem"
  value       = aws_efs_file_system.main.id
}

output "filesystem_arn" {
  description = "ARN of the EFS filesystem"
  value       = aws_efs_file_system.main.arn
}

output "dns_name" {
  description = "DNS name for the EFS filesystem"
  value       = aws_efs_file_system.main.dns_name
}

output "mount_target_ids" {
  description = "IDs of EFS mount targets"
  value       = aws_efs_mount_target.main[*].id
}

output "mount_target_ips" {
  description = "IP addresses of EFS mount targets"
  value       = aws_efs_mount_target.main[*].ip_address
}

output "access_point_id" {
  description = "ID of the EFS access point for state storage"
  value       = aws_efs_access_point.state.id
}

output "access_point_arn" {
  description = "ARN of the EFS access point"
  value       = aws_efs_access_point.state.arn
}

output "security_group_id" {
  description = "ID of the EFS security group"
  value       = aws_security_group.efs.id
}
