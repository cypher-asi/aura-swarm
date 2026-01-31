#!/bin/bash
# 05-configure-eks.sh - Configure EKS cluster
#
# Configures:
# - kubectl with cluster credentials
# - AWS EFS CSI Driver
# - Kata Containers runtime (or kata-qemu for dev)
# - RuntimeClass for kata-fc

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Configure EKS Cluster"
echo "=============================================="
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Update kubeconfig
#------------------------------------------------------------------------------

echo "Updating kubeconfig..."

aws eks update-kubeconfig \
    --region "${AWS_REGION}" \
    --name "${EKS_CLUSTER_NAME}"

echo -e "${GREEN}✓${NC} kubeconfig updated"
echo ""

# Verify connection
echo "Verifying cluster connection..."
if kubectl cluster-info &> /dev/null; then
    echo -e "${GREEN}✓${NC} Connected to cluster"
    kubectl get nodes
else
    echo -e "${RED}✗${NC} Failed to connect to cluster"
    exit 1
fi

echo ""

#------------------------------------------------------------------------------
# Install AWS EFS CSI Driver
#------------------------------------------------------------------------------

echo "Installing AWS EFS CSI Driver..."

# Get OIDC provider URL
OIDC_PROVIDER=$(terraform output -json | jq -r '.eks_oidc_provider_url.value // empty')

if [[ -z "$OIDC_PROVIDER" ]]; then
    echo -e "${RED}✗${NC} Could not get OIDC provider URL"
    exit 1
fi

# Create IAM role for EFS CSI driver
EFS_ROLE_NAME="${RESOURCE_PREFIX}-efs-csi-role"

# Check if role exists
if ! aws iam get-role --role-name "${EFS_ROLE_NAME}" &> /dev/null; then
    echo "Creating IAM role for EFS CSI driver..."
    
    cat > /tmp/efs-csi-trust-policy.json <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::$(aws sts get-caller-identity --query Account --output text):oidc-provider/${OIDC_PROVIDER}"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "${OIDC_PROVIDER}:aud": "sts.amazonaws.com",
          "${OIDC_PROVIDER}:sub": "system:serviceaccount:kube-system:efs-csi-controller-sa"
        }
      }
    }
  ]
}
EOF
    
    aws iam create-role \
        --role-name "${EFS_ROLE_NAME}" \
        --assume-role-policy-document file:///tmp/efs-csi-trust-policy.json
    
    aws iam attach-role-policy \
        --role-name "${EFS_ROLE_NAME}" \
        --policy-arn arn:aws:iam::aws:policy/service-role/AmazonEFSCSIDriverPolicy
    
    echo -e "${GREEN}✓${NC} IAM role created: ${EFS_ROLE_NAME}"
fi

EFS_ROLE_ARN="arn:aws:iam::$(aws sts get-caller-identity --query Account --output text):role/${EFS_ROLE_NAME}"

# Install EFS CSI driver using EKS add-on
echo "Installing EFS CSI driver add-on..."

if aws eks describe-addon --cluster-name "${EKS_CLUSTER_NAME}" --addon-name aws-efs-csi-driver &> /dev/null; then
    echo -e "${YELLOW}⚠${NC} EFS CSI driver already installed"
else
    aws eks create-addon \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --addon-name aws-efs-csi-driver \
        --service-account-role-arn "${EFS_ROLE_ARN}" \
        --resolve-conflicts OVERWRITE
    
    echo "Waiting for EFS CSI driver to be ready..."
    aws eks wait addon-active \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --addon-name aws-efs-csi-driver
    
    echo -e "${GREEN}✓${NC} EFS CSI driver installed"
fi

echo ""

#------------------------------------------------------------------------------
# Install Kata Containers Runtime
#------------------------------------------------------------------------------

echo "Installing Kata Containers runtime..."
echo ""
echo -e "${YELLOW}Note: For production, Kata with Firecracker requires bare metal"
echo "or instances with nested virtualization. For dev/testing, using"
echo "kata-qemu handler on standard instances.${NC}"
echo ""

# Apply Kata runtime class
# In production, you would install Kata Containers on nodes via:
# - Custom AMI with Kata pre-installed
# - DaemonSet that installs Kata
# For now, we create the RuntimeClass that will be used

cat <<EOF | kubectl apply -f -
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kata-fc
handler: kata-fc
overhead:
  podFixed:
    memory: "160Mi"
    cpu: "250m"
scheduling:
  nodeSelector:
    katacontainers.io/kata-runtime: "true"
---
# Alternative for dev/testing on standard instances
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kata-qemu
handler: kata-qemu
overhead:
  podFixed:
    memory: "160Mi"
    cpu: "250m"
EOF

echo -e "${GREEN}✓${NC} RuntimeClasses created (kata-fc, kata-qemu)"
echo ""

echo -e "${YELLOW}Note: You need to install Kata Containers on worker nodes.${NC}"
echo "Options:"
echo "  1. Use a custom EKS AMI with Kata pre-installed"
echo "  2. Deploy Kata via DaemonSet (see kata-containers.io docs)"
echo "  3. Use kata-qemu for testing without Firecracker"
echo ""

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------

echo ""
echo -e "${GREEN}=============================================="
echo "  EKS cluster configured!"
echo "==============================================${NC}"
echo ""
echo "Installed components:"
echo "  - kubeconfig updated"
echo "  - AWS EFS CSI Driver"
echo "  - RuntimeClasses (kata-fc, kata-qemu)"
echo ""
echo "Cluster nodes:"
kubectl get nodes -o wide
echo ""
echo "Next step: Run ./06-deploy-ecr.sh"
