#!/bin/bash
# 05-configure-eks.sh - Configure EKS cluster
#
# Configures:
# - kubectl with cluster credentials
# - AWS EFS CSI Driver
# - RuntimeClasses (kata-qemu, kata-qemu-snp)
# - Confidential Containers (CoCo) operator for the SEV-SNP node pool

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

# Get OIDC provider URL directly from the cluster (single source of truth)
OIDC_ISSUER_URL=$(aws eks describe-cluster \
    --name "${EKS_CLUSTER_NAME}" \
    --query "cluster.identity.oidc.issuer" \
    --output text)
OIDC_PROVIDER="${OIDC_ISSUER_URL#https://}"

if [[ -z "$OIDC_PROVIDER" ]]; then
    echo -e "${RED}✗${NC} Could not get OIDC provider URL from cluster"
    exit 1
fi

echo "  OIDC provider: ${OIDC_PROVIDER}"

EFS_ROLE_NAME="${RESOURCE_PREFIX}-efs-csi-role"
AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
EFS_ROLE_ARN="arn:aws:iam::${AWS_ACCOUNT_ID}:role/${EFS_ROLE_NAME}"

# Build trust policy — uses StringLike to cover all efs-csi-* service accounts
TRUST_POLICY=$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::${AWS_ACCOUNT_ID}:oidc-provider/${OIDC_PROVIDER}"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringLike": {
          "${OIDC_PROVIDER}:sub": "system:serviceaccount:kube-system:efs-csi-*",
          "${OIDC_PROVIDER}:aud": "sts.amazonaws.com"
        }
      }
    }
  ]
}
EOF
)

# Create or update the IAM role (always reconcile the trust policy so a
# cluster rebuild with a new OIDC issuer doesn't leave a stale policy)
if aws iam get-role --role-name "${EFS_ROLE_NAME}" &>/dev/null; then
    echo "  IAM role exists — updating trust policy to match current cluster..."
    aws iam update-assume-role-policy \
        --role-name "${EFS_ROLE_NAME}" \
        --policy-document "$TRUST_POLICY"
    echo -e "  ${GREEN}✓${NC} Trust policy updated"
else
    echo "  Creating IAM role ${EFS_ROLE_NAME}..."
    aws iam create-role \
        --role-name "${EFS_ROLE_NAME}" \
        --assume-role-policy-document "$TRUST_POLICY"
    echo -e "  ${GREEN}✓${NC} IAM role created"
fi

# Ensure the EFS policy is attached (idempotent)
aws iam attach-role-policy \
    --role-name "${EFS_ROLE_NAME}" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AmazonEFSCSIDriverPolicy 2>/dev/null || true

# Install or update the EFS CSI driver EKS add-on
echo "  Configuring EFS CSI driver add-on..."

if aws eks describe-addon --cluster-name "${EKS_CLUSTER_NAME}" --addon-name aws-efs-csi-driver &>/dev/null; then
    echo -e "  ${YELLOW}⚠${NC} Add-on already exists — ensuring role ARN is correct..."
    aws eks update-addon \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --addon-name aws-efs-csi-driver \
        --service-account-role-arn "${EFS_ROLE_ARN}" \
        --resolve-conflicts OVERWRITE
else
    aws eks create-addon \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --addon-name aws-efs-csi-driver \
        --service-account-role-arn "${EFS_ROLE_ARN}" \
        --resolve-conflicts OVERWRITE
fi

echo "  Waiting for EFS CSI driver to become active..."
aws eks wait addon-active \
    --cluster-name "${EKS_CLUSTER_NAME}" \
    --addon-name aws-efs-csi-driver

echo -e "  ${GREEN}✓${NC} EFS CSI driver active"

# Validate IRSA: verify the CSI controller SA is annotated with the correct role
echo "  Validating IRSA configuration..."

CSI_SA_ROLE=$(kubectl get sa efs-csi-controller-sa -n kube-system \
    -o jsonpath='{.metadata.annotations.eks\.amazonaws\.com/role-arn}' 2>/dev/null || echo "")

if [[ "$CSI_SA_ROLE" == "$EFS_ROLE_ARN" ]]; then
    echo -e "  ${GREEN}✓${NC} Service account annotated with correct role"
else
    echo -e "  ${YELLOW}⚠${NC} SA role annotation: '${CSI_SA_ROLE}' (expected '${EFS_ROLE_ARN}')"
    echo "    The EKS add-on should set this automatically. If PVC provisioning"
    echo "    fails later, re-run this script or check the add-on configuration."
fi

echo ""

#------------------------------------------------------------------------------
# Install Kata RuntimeClasses
#------------------------------------------------------------------------------

echo "Applying Kata RuntimeClasses..."

kubectl apply -f "${SCRIPT_DIR}/k8s/09-runtime-class.yaml"

# R3 cleanup: the retired kata-fc RuntimeClass is removed from clusters
# that still carry it (no agent pods reference it anymore).
kubectl delete runtimeclass kata-fc --ignore-not-found

echo -e "${GREEN}✓${NC} RuntimeClasses created (kata-qemu, kata-qemu-snp)"
echo ""

echo "The confidential (SEV-SNP) node pool handler install: the CoCo"
echo "operator installed below deploys kata-qemu-snp onto nodes labeled"
echo "swarm.io/confidential-node=true via the CcRuntime CR (10-coco-ccruntime.yaml)."
echo ""

#------------------------------------------------------------------------------
# Install Confidential Containers (CoCo) Operator
#------------------------------------------------------------------------------

# Pinned operator release. NOTE: the operator is superseded upstream by the
# confidential-containers Helm chart; migration is deferred to R3 cleanup.
COCO_OPERATOR_VERSION="${COCO_OPERATOR_VERSION:-v0.17.0}"

echo "Installing Confidential Containers operator (${COCO_OPERATOR_VERSION})..."

kubectl apply -k "github.com/confidential-containers/operator/config/default?ref=${COCO_OPERATOR_VERSION}"

echo "  Waiting for CoCo operator controller to become ready..."
kubectl rollout status deployment/cc-operator-controller-manager \
    -n confidential-containers-system --timeout=300s

echo -e "${GREEN}✓${NC} CoCo operator installed"
echo ""
echo "The CcRuntime CR (kata-qemu-snp install on SNP nodes) is applied by"
echo "./08-deploy-k8s.sh together with the rest of the manifests."
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
echo "  - RuntimeClasses (kata-qemu, kata-qemu-snp)"
echo "  - Confidential Containers operator (${COCO_OPERATOR_VERSION})"
echo ""
echo "Cluster nodes:"
kubectl get nodes -o wide
echo ""
echo "Next step: Run ./06-deploy-ecr.sh"
