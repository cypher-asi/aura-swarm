#!/bin/bash
# 04-deploy-eks.sh - Deploy EKS cluster
#
# Creates:
# - EKS cluster (v1.31)
# - IAM roles for cluster and nodes
# - Managed node group with Kata-capable instances
# - OIDC provider for IRSA

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Deploy EKS Cluster"
echo "=============================================="
echo ""
echo -e "${YELLOW}Note: EKS cluster creation takes 10-15 minutes${NC}"
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Update tfvars to enable EKS
#------------------------------------------------------------------------------

TFVARS_FILE="${SCRIPT_DIR}/terraform/terraform.tfvars"

# Update enable_eks to true
if grep -q "enable_eks" "${TFVARS_FILE}"; then
    sed -i 's/enable_eks     = false/enable_eks     = true/' "${TFVARS_FILE}"
else
    echo "enable_eks = true" >> "${TFVARS_FILE}"
fi

echo "Updated terraform.tfvars to enable EKS"
echo ""

#------------------------------------------------------------------------------
# Plan and apply
#------------------------------------------------------------------------------

echo "Planning EKS infrastructure..."
echo ""

terraform plan -out=eks.tfplan

echo ""
echo "=============================================="
echo -e "${YELLOW}Review the plan above before proceeding${NC}"
echo ""
echo "This will create:"
echo "  - EKS cluster (${EKS_CLUSTER_NAME})"
echo "  - Managed node group (${NODE_DESIRED_COUNT} x ${NODE_INSTANCE_TYPE})"
echo "  - IAM roles and OIDC provider"
echo ""
echo "Estimated time: 10-15 minutes"
echo "=============================================="
echo ""
read -p "Apply this plan? (yes/no) [no]: " confirm
confirm=${confirm:-no}

if [[ "$confirm" != "yes" ]]; then
    echo "Aborted. Run this script again when ready."
    exit 0
fi

echo ""
echo "Applying EKS infrastructure..."
echo "This will take 10-15 minutes. Please wait..."
echo ""

terraform apply eks.tfplan

echo ""
echo -e "${GREEN}=============================================="
echo "  EKS cluster deployed successfully!"
echo "==============================================${NC}"
echo ""

# Show outputs
echo "EKS outputs:"
terraform output -json | jq -r '.eks_cluster_name.value // empty' | xargs -I {} echo "  Cluster Name: {}"
terraform output -json | jq -r '.eks_cluster_endpoint.value // empty' | xargs -I {} echo "  Cluster Endpoint: {}"

echo ""
echo "Update kubeconfig:"
terraform output -json | jq -r '.eks_update_kubeconfig_command.value // empty'

echo ""
echo "Next step: Run ./05-configure-eks.sh"
