# Terraform Backend Configuration
#
# Uncomment and configure this block to use S3 backend for remote state.
# Run 01-init-terraform.sh to create the S3 bucket first.
#
# For local development, you can leave this commented out to use local state.

# terraform {
#   backend "s3" {
#     bucket         = "aura-swarm-dev-terraform-state"
#     key            = "terraform.tfstate"
#     region         = "us-east-2"
#     encrypt        = true
#     dynamodb_table = "aura-swarm-dev-terraform-lock"
#   }
# }
