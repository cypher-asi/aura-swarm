#!/bin/bash
# 01-init-terraform.sh - Initialize Terraform backend and modules
#
# This script optionally creates an S3 bucket for Terraform state
# and initializes the Terraform working directory.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Initialize Terraform"
echo "=============================================="
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Option: Create S3 backend for remote state
#------------------------------------------------------------------------------

create_s3_backend() {
    echo "Creating S3 bucket for Terraform state..."
    
    # Check if bucket already exists
    if aws s3 ls "s3://${TF_STATE_BUCKET}" 2>&1 | grep -q "NoSuchBucket"; then
        echo "Creating bucket: ${TF_STATE_BUCKET}"
        
        if [[ "${AWS_REGION}" == "us-east-1" ]]; then
            aws s3api create-bucket \
                --bucket "${TF_STATE_BUCKET}" \
                --region "${AWS_REGION}"
        else
            aws s3api create-bucket \
                --bucket "${TF_STATE_BUCKET}" \
                --region "${AWS_REGION}" \
                --create-bucket-configuration LocationConstraint="${AWS_REGION}"
        fi
        
        # Enable versioning
        aws s3api put-bucket-versioning \
            --bucket "${TF_STATE_BUCKET}" \
            --versioning-configuration Status=Enabled
        
        # Enable encryption
        aws s3api put-bucket-encryption \
            --bucket "${TF_STATE_BUCKET}" \
            --server-side-encryption-configuration '{"Rules": [{"ApplyServerSideEncryptionByDefault": {"SSEAlgorithm": "AES256"}}]}'
        
        # Block public access
        aws s3api put-public-access-block \
            --bucket "${TF_STATE_BUCKET}" \
            --public-access-block-configuration "BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true"
        
        echo -e "${GREEN}✓${NC} S3 bucket created: ${TF_STATE_BUCKET}"
    else
        echo -e "${YELLOW}⚠${NC} S3 bucket already exists: ${TF_STATE_BUCKET}"
    fi
    
    # Create DynamoDB table for state locking
    local lock_table="${RESOURCE_PREFIX}-terraform-lock"
    
    if ! aws dynamodb describe-table --table-name "${lock_table}" --region "${AWS_REGION}" &> /dev/null; then
        echo "Creating DynamoDB table for state locking: ${lock_table}"
        
        aws dynamodb create-table \
            --table-name "${lock_table}" \
            --attribute-definitions AttributeName=LockID,AttributeType=S \
            --key-schema AttributeName=LockID,KeyType=HASH \
            --billing-mode PAY_PER_REQUEST \
            --region "${AWS_REGION}"
        
        echo -e "${GREEN}✓${NC} DynamoDB table created: ${lock_table}"
    else
        echo -e "${YELLOW}⚠${NC} DynamoDB table already exists: ${lock_table}"
    fi
}

#------------------------------------------------------------------------------
# Ask user about backend
#------------------------------------------------------------------------------

echo "Terraform state can be stored locally or in S3."
echo ""
echo "Options:"
echo "  1) Local state (default, for development)"
echo "  2) S3 remote state (recommended for teams/production)"
echo ""
read -p "Select option [1]: " backend_choice
backend_choice=${backend_choice:-1}

if [[ "$backend_choice" == "2" ]]; then
    create_s3_backend
    
    echo ""
    echo "To use S3 backend, uncomment the backend block in backend.tf"
    echo "and update it with:"
    echo ""
    echo "  bucket         = \"${TF_STATE_BUCKET}\""
    echo "  key            = \"${TF_STATE_KEY}\""
    echo "  region         = \"${TF_STATE_REGION}\""
    echo "  dynamodb_table = \"${RESOURCE_PREFIX}-terraform-lock\""
    echo ""
fi

#------------------------------------------------------------------------------
# Initialize Terraform
#------------------------------------------------------------------------------

echo ""
echo "Initializing Terraform..."
echo ""

terraform init -upgrade

echo ""
echo -e "${GREEN}=============================================="
echo "  Terraform initialized successfully!"
echo "==============================================${NC}"
echo ""
echo "Next step: Run ./02-deploy-network.sh"
