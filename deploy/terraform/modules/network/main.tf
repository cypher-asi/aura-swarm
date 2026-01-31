#------------------------------------------------------------------------------
# Network Module - VPC, Subnets, NAT Gateway
#
# Creates the network topology from spec 08-networking.md:
# - VPC (10.0.0.0/16)
# - Public subnet (10.0.1.0/24) for ALB
# - Private subnet (10.0.2.0/24) for platform services
# - Agent subnet (10.0.3.0/24) for microVM pods
# - Storage subnet (10.0.4.0/24) for EFS
#------------------------------------------------------------------------------

#------------------------------------------------------------------------------
# VPC
#------------------------------------------------------------------------------

resource "aws_vpc" "main" {
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-vpc"
  })
}

#------------------------------------------------------------------------------
# Internet Gateway
#------------------------------------------------------------------------------

resource "aws_internet_gateway" "main" {
  vpc_id = aws_vpc.main.id

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-igw"
  })
}

#------------------------------------------------------------------------------
# Elastic IP for NAT Gateway
#------------------------------------------------------------------------------

resource "aws_eip" "nat" {
  domain = "vpc"

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-nat-eip"
  })

  depends_on = [aws_internet_gateway.main]
}

#------------------------------------------------------------------------------
# NAT Gateway (in first public subnet)
#------------------------------------------------------------------------------

resource "aws_nat_gateway" "main" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public[0].id

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-nat"
  })

  depends_on = [aws_internet_gateway.main]
}

#------------------------------------------------------------------------------
# Public Subnets (for load balancers)
#------------------------------------------------------------------------------

resource "aws_subnet" "public" {
  count = length(var.availability_zones)

  vpc_id                  = aws_vpc.main.id
  cidr_block              = cidrsubnet(var.public_subnet_cidr, 1, count.index)
  availability_zone       = var.availability_zones[count.index]
  map_public_ip_on_launch = true

  tags = merge(var.tags, {
    Name                                           = "${var.resource_prefix}-public-${var.availability_zones[count.index]}"
    "kubernetes.io/role/elb"                       = "1"
    "kubernetes.io/cluster/${var.resource_prefix}-cluster" = "shared"
  })
}

#------------------------------------------------------------------------------
# Private Subnets (for platform services)
#------------------------------------------------------------------------------

resource "aws_subnet" "private" {
  count = length(var.availability_zones)

  vpc_id            = aws_vpc.main.id
  cidr_block        = cidrsubnet(var.private_subnet_cidr, 1, count.index)
  availability_zone = var.availability_zones[count.index]

  tags = merge(var.tags, {
    Name                                           = "${var.resource_prefix}-private-${var.availability_zones[count.index]}"
    "kubernetes.io/role/internal-elb"              = "1"
    "kubernetes.io/cluster/${var.resource_prefix}-cluster" = "shared"
  })
}

#------------------------------------------------------------------------------
# Agent Subnets (for microVM pods)
#------------------------------------------------------------------------------

resource "aws_subnet" "agent" {
  count = length(var.availability_zones)

  vpc_id            = aws_vpc.main.id
  cidr_block        = cidrsubnet(var.agent_subnet_cidr, 1, count.index)
  availability_zone = var.availability_zones[count.index]

  tags = merge(var.tags, {
    Name                                           = "${var.resource_prefix}-agent-${var.availability_zones[count.index]}"
    "kubernetes.io/cluster/${var.resource_prefix}-cluster" = "shared"
  })
}

#------------------------------------------------------------------------------
# Storage Subnets (for EFS mount targets)
#------------------------------------------------------------------------------

resource "aws_subnet" "storage" {
  count = length(var.availability_zones)

  vpc_id            = aws_vpc.main.id
  cidr_block        = cidrsubnet(var.storage_subnet_cidr, 1, count.index)
  availability_zone = var.availability_zones[count.index]

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-storage-${var.availability_zones[count.index]}"
  })
}

#------------------------------------------------------------------------------
# Route Tables
#------------------------------------------------------------------------------

# Public route table (routes to internet gateway)
resource "aws_route_table" "public" {
  vpc_id = aws_vpc.main.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.main.id
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-public-rt"
  })
}

# Private route table (routes to NAT gateway)
resource "aws_route_table" "private" {
  vpc_id = aws_vpc.main.id

  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.main.id
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-private-rt"
  })
}

#------------------------------------------------------------------------------
# Route Table Associations
#------------------------------------------------------------------------------

resource "aws_route_table_association" "public" {
  count = length(var.availability_zones)

  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table_association" "private" {
  count = length(var.availability_zones)

  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private.id
}

resource "aws_route_table_association" "agent" {
  count = length(var.availability_zones)

  subnet_id      = aws_subnet.agent[count.index].id
  route_table_id = aws_route_table.private.id
}

resource "aws_route_table_association" "storage" {
  count = length(var.availability_zones)

  subnet_id      = aws_subnet.storage[count.index].id
  route_table_id = aws_route_table.private.id
}
