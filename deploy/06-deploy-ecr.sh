#!/bin/bash
# 06-deploy-ecr.sh - Deploy ECR repositories
#
# Creates ECR repositories:
# - aura-swarm-gateway
# - aura-swarm-control
# - aura-swarm-scheduler

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Deploy ECR Repositories"
echo "=============================================="
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Update tfvars to enable ECR
#------------------------------------------------------------------------------

TFVARS_FILE="${SCRIPT_DIR}/terraform/terraform.tfvars"

# Update enable_ecr to true
if grep -q "enable_ecr" "${TFVARS_FILE}"; then
    sed -i 's/enable_ecr     = false/enable_ecr     = true/' "${TFVARS_FILE}"
else
    echo "enable_ecr = true" >> "${TFVARS_FILE}"
fi

echo "Updated terraform.tfvars to enable ECR"
echo ""

#------------------------------------------------------------------------------
# Plan and apply
#------------------------------------------------------------------------------

echo "Planning ECR infrastructure..."
echo ""

terraform plan -out=ecr.tfplan

echo ""
echo "=============================================="
echo -e "${YELLOW}Review the plan above before proceeding${NC}"
echo "=============================================="
echo ""
read -p "Apply this plan? (yes/no) [no]: " confirm
confirm=${confirm:-no}

if [[ "$confirm" != "yes" ]]; then
    echo "Aborted. Run this script again when ready."
    exit 0
fi

echo ""
echo "Applying ECR infrastructure..."
echo ""

terraform apply ecr.tfplan

echo ""
echo -e "${GREEN}=============================================="
echo "  ECR repositories deployed!"
echo "==============================================${NC}"
echo ""

# Show outputs
echo "ECR repositories:"
terraform output -json | jq -r '.ecr_repository_urls.value // {} | to_entries[] | "  \(.key): \(.value)"'

echo ""

# Get ECR login command
AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
ECR_REGISTRY="${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"

echo "To authenticate Docker with ECR:"
echo ""
echo "  aws ecr get-login-password --region ${AWS_REGION} | docker login --username AWS --password-stdin ${ECR_REGISTRY}"
echo ""
echo "Next step: Run ./07-build-images.sh"
