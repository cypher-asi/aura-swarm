#!/bin/bash
# destroy.sh - Tear down all infrastructure
#
# WARNING: This will destroy ALL resources including:
# - Kubernetes deployments
# - EKS cluster
# - EFS filesystem (and all data)
# - ECR repositories (and all images)
# - VPC and networking

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo -e "${RED}  DESTROY ALL INFRASTRUCTURE${NC}"
echo "=============================================="
echo ""
echo -e "${RED}WARNING: This will permanently destroy:${NC}"
echo "  - All Kubernetes resources"
echo "  - EKS cluster and node groups"
echo "  - EFS filesystem (ALL DATA WILL BE LOST)"
echo "  - ECR repositories (ALL IMAGES WILL BE DELETED)"
echo "  - VPC and all networking"
echo ""
echo "Project: ${PROJECT_NAME}"
echo "Environment: ${ENVIRONMENT}"
echo "Region: ${AWS_REGION}"
echo ""
echo -e "${RED}THIS CANNOT BE UNDONE!${NC}"
echo ""
read -p "Type 'destroy ${ENVIRONMENT}' to confirm: " confirm

if [[ "$confirm" != "destroy ${ENVIRONMENT}" ]]; then
    echo "Aborted. No changes made."
    exit 0
fi

echo ""
echo "Starting destruction..."
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Delete Kubernetes resources first
#------------------------------------------------------------------------------

echo "Deleting Kubernetes resources..."

# Try to delete K8s resources if cluster is accessible
if kubectl cluster-info &> /dev/null 2>&1; then
    echo "Deleting deployments..."
    kubectl delete -f "${SCRIPT_DIR}/k8s/" --ignore-not-found=true 2>/dev/null || true
    
    echo "Waiting for pods to terminate..."
    kubectl delete pods --all -n "${K8S_NAMESPACE_SYSTEM}" --wait=false 2>/dev/null || true
    kubectl delete pods --all -n "${K8S_NAMESPACE_AGENTS}" --wait=false 2>/dev/null || true
    
    # Delete namespaces (this will delete everything in them)
    kubectl delete namespace "${K8S_NAMESPACE_SYSTEM}" --wait=false 2>/dev/null || true
    kubectl delete namespace "${K8S_NAMESPACE_AGENTS}" --wait=false 2>/dev/null || true
    
    echo -e "${GREEN}✓${NC} Kubernetes resources deleted"
else
    echo -e "${YELLOW}⚠${NC} Could not connect to cluster, skipping K8s cleanup"
fi

echo ""

#------------------------------------------------------------------------------
# Delete EFS CSI driver IAM role
#------------------------------------------------------------------------------

echo "Cleaning up IAM resources..."

EFS_ROLE_NAME="${RESOURCE_PREFIX}-efs-csi-role"

if aws iam get-role --role-name "${EFS_ROLE_NAME}" &> /dev/null; then
    echo "Detaching policies from ${EFS_ROLE_NAME}..."
    aws iam detach-role-policy \
        --role-name "${EFS_ROLE_NAME}" \
        --policy-arn arn:aws:iam::aws:policy/service-role/AmazonEFSCSIDriverPolicy 2>/dev/null || true
    
    echo "Deleting role ${EFS_ROLE_NAME}..."
    aws iam delete-role --role-name "${EFS_ROLE_NAME}" 2>/dev/null || true
    
    echo -e "${GREEN}✓${NC} IAM role deleted"
fi

echo ""

#------------------------------------------------------------------------------
# Empty ECR repositories before deletion
#------------------------------------------------------------------------------

echo "Emptying ECR repositories..."

AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
REPOS=("gateway" "control" "scheduler")

for repo in "${REPOS[@]}"; do
    REPO_NAME="${RESOURCE_PREFIX}-${repo}"
    
    # Get all image digests
    IMAGES=$(aws ecr list-images --repository-name "${REPO_NAME}" --query 'imageIds[*]' --output json 2>/dev/null || echo "[]")
    
    if [[ "$IMAGES" != "[]" ]]; then
        echo "Deleting images in ${REPO_NAME}..."
        aws ecr batch-delete-image \
            --repository-name "${REPO_NAME}" \
            --image-ids "${IMAGES}" 2>/dev/null || true
    fi
done

echo -e "${GREEN}✓${NC} ECR repositories emptied"
echo ""

#------------------------------------------------------------------------------
# Terraform destroy
#------------------------------------------------------------------------------

echo "Running Terraform destroy..."
echo ""

terraform destroy -auto-approve

echo ""

#------------------------------------------------------------------------------
# Cleanup Terraform state bucket (optional)
#------------------------------------------------------------------------------

echo ""
read -p "Delete Terraform state bucket? (yes/no) [no]: " delete_bucket
delete_bucket=${delete_bucket:-no}

if [[ "$delete_bucket" == "yes" ]]; then
    echo "Emptying S3 bucket ${TF_STATE_BUCKET}..."
    aws s3 rm "s3://${TF_STATE_BUCKET}" --recursive 2>/dev/null || true
    
    echo "Deleting S3 bucket ${TF_STATE_BUCKET}..."
    aws s3api delete-bucket --bucket "${TF_STATE_BUCKET}" --region "${AWS_REGION}" 2>/dev/null || true
    
    echo "Deleting DynamoDB table ${RESOURCE_PREFIX}-terraform-lock..."
    aws dynamodb delete-table --table-name "${RESOURCE_PREFIX}-terraform-lock" --region "${AWS_REGION}" 2>/dev/null || true
    
    echo -e "${GREEN}✓${NC} Terraform state resources deleted"
fi

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------

echo ""
echo -e "${GREEN}=============================================="
echo "  Infrastructure destroyed"
echo "==============================================${NC}"
echo ""
echo "All resources have been deleted."
echo "The following were removed:"
echo "  - EKS cluster: ${EKS_CLUSTER_NAME}"
echo "  - VPC and networking"
echo "  - EFS filesystem"
echo "  - ECR repositories"
echo ""
