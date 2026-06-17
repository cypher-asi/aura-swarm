#!/bin/bash
# 03-deploy-storage.sh - Deploy EFS filesystem
#
# Creates:
# - EFS filesystem with encryption
# - Mount targets in each AZ
# - Security group for NFS traffic
# - Access point for state storage

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"
source "${SCRIPT_DIR}/_helpers.sh"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  AURA Swarm - Deploy Storage"
echo "=============================================="
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Update tfvars to enable storage
#------------------------------------------------------------------------------

TFVARS_FILE="${SCRIPT_DIR}/terraform/terraform.tfvars"

# Update enable_storage to true
if grep -q "enable_storage" "${TFVARS_FILE}"; then
    sed -i 's/enable_storage = false/enable_storage = true/' "${TFVARS_FILE}"
else
    echo "enable_storage = true" >> "${TFVARS_FILE}"
fi

echo "Updated terraform.tfvars to enable storage"
echo ""

#------------------------------------------------------------------------------
# Plan and apply
#------------------------------------------------------------------------------

echo "Planning storage infrastructure..."
echo ""

terraform plan -out=storage.tfplan

confirm_plan "storage.tfplan"

echo ""
echo "Applying storage infrastructure..."
echo ""

terraform apply storage.tfplan

echo ""
echo -e "${GREEN}=============================================="
echo "  Storage deployed successfully!"
echo "==============================================${NC}"
echo ""

# Show outputs
echo "Storage outputs:"
terraform output -json | jq -r '.efs_filesystem_id.value // empty' | xargs -I {} echo "  EFS Filesystem ID: {}"
terraform output -json | jq -r '.efs_dns_name.value // empty' | xargs -I {} echo "  EFS DNS Name: {}"

echo ""
echo "Next step: Run ./04-deploy-eks.sh"
