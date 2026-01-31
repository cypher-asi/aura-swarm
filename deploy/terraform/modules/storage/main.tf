#------------------------------------------------------------------------------
# Storage Module - EFS Filesystem
#
# Creates EFS filesystem for agent persistent storage as specified in
# spec 01-system-overview.md section 6.2 (Agent State Storage).
#
# Storage layout: /state/<user_id>/<agent_id>/
#------------------------------------------------------------------------------

#------------------------------------------------------------------------------
# Security Group for EFS
#------------------------------------------------------------------------------

resource "aws_security_group" "efs" {
  name        = "${var.resource_prefix}-efs-sg"
  description = "Security group for EFS mount targets"
  vpc_id      = var.vpc_id

  # Allow NFS from agent subnets
  ingress {
    description = "NFS from agent subnets"
    from_port   = 2049
    to_port     = 2049
    protocol    = "tcp"
    cidr_blocks = var.agent_subnet_cidrs
  }

  # Allow NFS from private subnets (for platform services if needed)
  ingress {
    description = "NFS from private subnets"
    from_port   = 2049
    to_port     = 2049
    protocol    = "tcp"
    cidr_blocks = var.agent_subnet_cidrs
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-efs-sg"
  })
}

#------------------------------------------------------------------------------
# EFS Filesystem
#------------------------------------------------------------------------------

resource "aws_efs_file_system" "main" {
  creation_token = "${var.resource_prefix}-efs"
  encrypted      = var.encrypted

  performance_mode = var.performance_mode
  throughput_mode  = var.throughput_mode

  lifecycle_policy {
    transition_to_ia = "AFTER_30_DAYS"
  }

  lifecycle_policy {
    transition_to_primary_storage_class = "AFTER_1_ACCESS"
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-efs"
  })
}

#------------------------------------------------------------------------------
# EFS Mount Targets (one per storage subnet)
#------------------------------------------------------------------------------

resource "aws_efs_mount_target" "main" {
  count = length(var.subnet_ids)

  file_system_id  = aws_efs_file_system.main.id
  subnet_id       = var.subnet_ids[count.index]
  security_groups = [aws_security_group.efs.id]
}

#------------------------------------------------------------------------------
# EFS Access Point (for organized agent storage)
#------------------------------------------------------------------------------

resource "aws_efs_access_point" "state" {
  file_system_id = aws_efs_file_system.main.id

  root_directory {
    path = "/state"
    creation_info {
      owner_gid   = 1000
      owner_uid   = 1000
      permissions = "0755"
    }
  }

  posix_user {
    gid = 1000
    uid = 1000
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-efs-ap-state"
  })
}

#------------------------------------------------------------------------------
# EFS Backup Policy
#------------------------------------------------------------------------------

resource "aws_efs_backup_policy" "main" {
  file_system_id = aws_efs_file_system.main.id

  backup_policy {
    status = "ENABLED"
  }
}
