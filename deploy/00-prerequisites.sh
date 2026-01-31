#!/bin/bash
# 00-prerequisites.sh - Verify all required tools and AWS authentication
#
# This script checks that all necessary tools are installed and AWS credentials
# are properly configured before attempting infrastructure deployment.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

ERRORS=0

echo "=============================================="
echo "  Aura Swarm - Prerequisites Check"
echo "=============================================="
echo ""

#------------------------------------------------------------------------------
# Helper functions
#------------------------------------------------------------------------------

check_command() {
    local cmd=$1
    local min_version=${2:-""}
    local install_hint=${3:-""}
    
    if command -v "$cmd" &> /dev/null; then
        local version
        case "$cmd" in
            aws)
                version=$(aws --version 2>&1 | cut -d/ -f2 | cut -d' ' -f1)
                ;;
            terraform)
                version=$(terraform version -json 2>/dev/null | grep -o '"terraform_version":"[^"]*"' | cut -d'"' -f4 || terraform version | head -1 | awk '{print $2}' | tr -d 'v')
                ;;
            kubectl)
                version=$(kubectl version --client -o json 2>/dev/null | grep -o '"gitVersion":"[^"]*"' | cut -d'"' -f4 | tr -d 'v' || kubectl version --client 2>&1 | head -1)
                ;;
            docker)
                version=$(docker version --format '{{.Client.Version}}' 2>/dev/null || echo "unknown")
                ;;
            helm)
                version=$(helm version --short 2>/dev/null | tr -d 'v' || echo "unknown")
                ;;
            *)
                version="installed"
                ;;
        esac
        echo -e "${GREEN}✓${NC} $cmd (version: $version)"
        return 0
    else
        echo -e "${RED}✗${NC} $cmd is not installed"
        if [[ -n "$install_hint" ]]; then
            echo "  Install: $install_hint"
        fi
        ((ERRORS++))
        return 1
    fi
}

check_aws_auth() {
    echo ""
    echo "Checking AWS authentication..."
    
    if [[ -n "${AWS_PROFILE:-}" ]]; then
        echo "  Using AWS profile: ${AWS_PROFILE}"
    fi
    
    if aws sts get-caller-identity &> /dev/null; then
        local account_id=$(aws sts get-caller-identity --query Account --output text)
        local arn=$(aws sts get-caller-identity --query Arn --output text)
        echo -e "${GREEN}✓${NC} AWS authentication successful"
        echo "  Account: $account_id"
        echo "  Identity: $arn"
        echo "  Region: ${AWS_REGION}"
        return 0
    else
        echo -e "${RED}✗${NC} AWS authentication failed"
        echo "  Ensure AWS credentials are configured:"
        echo "    - Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or"
        echo "    - Configure AWS CLI profile with 'aws configure', or"
        echo "    - Set AWS_PROFILE to use a named profile"
        ((ERRORS++))
        return 1
    fi
}

check_docker_running() {
    echo ""
    echo "Checking Docker daemon..."
    
    if docker info &> /dev/null; then
        echo -e "${GREEN}✓${NC} Docker daemon is running"
        return 0
    else
        echo -e "${YELLOW}⚠${NC} Docker daemon is not running"
        echo "  Docker is required for building container images (step 07)"
        echo "  Start Docker if you plan to build images"
        return 0  # Warning only, not a hard failure
    fi
}

check_aws_region() {
    echo ""
    echo "Checking AWS region configuration..."
    
    if [[ -z "${AWS_REGION:-}" ]]; then
        echo -e "${RED}✗${NC} AWS_REGION is not set"
        echo "  Set AWS_REGION in config.env or export it before running"
        ((ERRORS++))
        return 1
    fi
    
    # Verify the region exists
    if aws ec2 describe-availability-zones --region "${AWS_REGION}" &> /dev/null; then
        echo -e "${GREEN}✓${NC} AWS region '${AWS_REGION}' is valid"
        return 0
    else
        echo -e "${RED}✗${NC} AWS region '${AWS_REGION}' is not valid or not accessible"
        ((ERRORS++))
        return 1
    fi
}

#------------------------------------------------------------------------------
# Main checks
#------------------------------------------------------------------------------

echo "Checking required tools..."
echo ""

# AWS CLI v2
check_command "aws" "2.0.0" "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html"

# Terraform
check_command "terraform" "1.5.0" "https://developer.hashicorp.com/terraform/downloads"

# kubectl
check_command "kubectl" "1.28.0" "https://kubernetes.io/docs/tasks/tools/"

# Docker
check_command "docker" "20.0.0" "https://docs.docker.com/get-docker/"

# Helm (optional but recommended)
check_command "helm" "3.0.0" "https://helm.sh/docs/intro/install/" || true

# jq (for JSON processing in scripts)
check_command "jq" "" "https://jqlang.github.io/jq/download/"

# Check AWS authentication
check_aws_auth

# Check AWS region
check_aws_region

# Check Docker daemon
check_docker_running

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------

echo ""
echo "=============================================="

if [[ $ERRORS -eq 0 ]]; then
    echo -e "${GREEN}All prerequisites satisfied!${NC}"
    echo ""
    echo "Configuration summary:"
    echo "  Project:     ${PROJECT_NAME}"
    echo "  Environment: ${ENVIRONMENT}"
    echo "  Region:      ${AWS_REGION}"
    echo "  EKS Version: ${EKS_VERSION}"
    echo ""
    echo "Next step: Run ./01-init-terraform.sh"
    exit 0
else
    echo -e "${RED}Prerequisites check failed with $ERRORS error(s)${NC}"
    echo ""
    echo "Please install the missing tools and configure AWS credentials"
    echo "before proceeding with the deployment."
    exit 1
fi
