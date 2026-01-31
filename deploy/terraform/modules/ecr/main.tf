#------------------------------------------------------------------------------
# ECR Module - Container Registries
#
# Creates ECR repositories for aura-swarm container images:
# - gateway
# - control
# - scheduler
#------------------------------------------------------------------------------

resource "aws_ecr_repository" "repos" {
  for_each = toset(var.repositories)

  name                 = "${var.resource_prefix}-${each.key}"
  image_tag_mutability = "MUTABLE"

  image_scanning_configuration {
    scan_on_push = true
  }

  encryption_configuration {
    encryption_type = "AES256"
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-${each.key}"
  })
}

#------------------------------------------------------------------------------
# Lifecycle Policy (retain N most recent images)
#------------------------------------------------------------------------------

resource "aws_ecr_lifecycle_policy" "repos" {
  for_each = aws_ecr_repository.repos

  repository = each.value.name

  policy = jsonencode({
    rules = [
      {
        rulePriority = 1
        description  = "Keep last ${var.image_retention_count} images"
        selection = {
          tagStatus     = "any"
          countType     = "imageCountMoreThan"
          countNumber   = var.image_retention_count
        }
        action = {
          type = "expire"
        }
      }
    ]
  })
}

#------------------------------------------------------------------------------
# Repository Policy (allow EKS nodes to pull images)
#------------------------------------------------------------------------------

data "aws_caller_identity" "current" {}

resource "aws_ecr_repository_policy" "repos" {
  for_each = aws_ecr_repository.repos

  repository = each.value.name

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "AllowPull"
        Effect = "Allow"
        Principal = {
          AWS = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:root"
        }
        Action = [
          "ecr:GetDownloadUrlForLayer",
          "ecr:BatchGetImage",
          "ecr:BatchCheckLayerAvailability"
        ]
      }
    ]
  })
}
