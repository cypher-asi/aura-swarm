output "vpc_id" {
  description = "ID of the VPC"
  value       = aws_vpc.main.id
}

output "vpc_cidr" {
  description = "CIDR block of the VPC"
  value       = aws_vpc.main.cidr_block
}

output "public_subnet_ids" {
  description = "IDs of public subnets"
  value       = aws_subnet.public[*].id
}

output "public_subnet_cidrs" {
  description = "CIDR blocks of public subnets"
  value       = aws_subnet.public[*].cidr_block
}

output "private_subnet_ids" {
  description = "IDs of private subnets"
  value       = aws_subnet.private[*].id
}

output "private_subnet_cidrs" {
  description = "CIDR blocks of private subnets"
  value       = aws_subnet.private[*].cidr_block
}

output "agent_subnet_ids" {
  description = "IDs of agent subnets"
  value       = aws_subnet.agent[*].id
}

output "agent_subnet_cidrs" {
  description = "CIDR blocks of agent subnets"
  value       = aws_subnet.agent[*].cidr_block
}

output "storage_subnet_ids" {
  description = "IDs of storage subnets"
  value       = aws_subnet.storage[*].id
}

output "storage_subnet_cidrs" {
  description = "CIDR blocks of storage subnets"
  value       = aws_subnet.storage[*].cidr_block
}

output "nat_gateway_ip" {
  description = "Public IP of NAT Gateway"
  value       = aws_eip.nat.public_ip
}

output "internet_gateway_id" {
  description = "ID of the Internet Gateway"
  value       = aws_internet_gateway.main.id
}
