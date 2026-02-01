#!/bin/bash
# 08-deploy-k8s.sh - Deploy Kubernetes manifests
#
# Deploys:
# - Namespaces (swarm-system, swarm-agents)
# - StorageClass, PVC
# - Secrets (placeholders), RBAC
# - Deployments (gateway, control, scheduler)
# - Network policies

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Deploy Kubernetes Manifests"
echo "=============================================="
echo ""

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# Get infrastructure values from Terraform
#------------------------------------------------------------------------------

echo "Reading infrastructure values from Terraform..."

EFS_ID=$(terraform output -json | jq -r '.efs_filesystem_id.value // empty')

if [[ -z "$EFS_ID" ]]; then
    echo -e "${RED}✗${NC} Could not get EFS filesystem ID"
    echo "Ensure storage was deployed (03-deploy-storage.sh)"
    exit 1
fi

AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
ECR_REGISTRY="${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"

echo "  EFS ID: ${EFS_ID}"
echo "  ECR Registry: ${ECR_REGISTRY}"
echo ""

#------------------------------------------------------------------------------
# Load secrets from .secrets/ folder
#------------------------------------------------------------------------------

SECRETS_DIR="${SCRIPT_DIR}/../.secrets"

echo "Loading secrets from .secrets/ folder..."

load_secret() {
    local name="$1"
    local file="${SECRETS_DIR}/${name}"
    
    if [[ -f "$file" ]]; then
        cat "$file" | tr -d '\n'
    else
        echo ""
    fi
}

ANTHROPIC_API_KEY=$(load_secret "ANTHROPIC_API_KEY")
OPENAI_API_KEY=$(load_secret "OPENAI_API_KEY")
ZERO_ID_SECRET=$(load_secret "ZERO_ID_SECRET")

# Validate required secrets
MISSING_SECRETS=()
if [[ -z "$ANTHROPIC_API_KEY" ]]; then
    MISSING_SECRETS+=("ANTHROPIC_API_KEY")
fi

if [[ ${#MISSING_SECRETS[@]} -gt 0 ]]; then
    echo -e "${RED}✗${NC} Missing required secrets: ${MISSING_SECRETS[*]}"
    echo ""
    echo "Create the following files in .secrets/ folder:"
    for secret in "${MISSING_SECRETS[@]}"; do
        echo "  - .secrets/${secret}"
    done
    echo ""
    echo "See .secrets/README.md for instructions."
    exit 1
fi

echo -e "${GREEN}✓${NC} Secrets loaded"
echo ""

#------------------------------------------------------------------------------
# Update K8s manifests with environment values
#------------------------------------------------------------------------------

K8S_DIR="${SCRIPT_DIR}/k8s"

echo "Updating Kubernetes manifests..."

# Update storage class with EFS ID
sed -i "s/EFS_FILESYSTEM_ID/${EFS_ID}/g" "${K8S_DIR}/01-storage-class.yaml" 2>/dev/null || true

# Update ConfigMap with aura-runtime image URL
RUNTIME_IMAGE="${ECR_REGISTRY}/${RESOURCE_PREFIX}-runtime:${IMAGE_TAG}"
sed -i "s|REPLACE_WITH_ECR_REGISTRY/${RESOURCE_PREFIX}-runtime:v0.1.0|${RUNTIME_IMAGE}|g" "${K8S_DIR}/03-secrets.yaml" 2>/dev/null || true

# Inject secrets into the secrets manifest (use a temp file to avoid partial writes)
SECRETS_YAML="${K8S_DIR}/03-secrets.yaml"
SECRETS_YAML_TMP="${K8S_DIR}/03-secrets.yaml.tmp"

cp "$SECRETS_YAML" "$SECRETS_YAML_TMP"
sed -i "s|__ANTHROPIC_API_KEY__|${ANTHROPIC_API_KEY}|g" "$SECRETS_YAML_TMP"
sed -i "s|__OPENAI_API_KEY__|${OPENAI_API_KEY:-placeholder-not-set}|g" "$SECRETS_YAML_TMP"
sed -i "s|__ZERO_ID_SECRET__|${ZERO_ID_SECRET:-placeholder-not-set}|g" "$SECRETS_YAML_TMP"
sed -i "s|__DEFAULT_ISOLATION__|${DEFAULT_ISOLATION}|g" "$SECRETS_YAML_TMP"

# Update deployments with ECR image URLs
for manifest in "${K8S_DIR}"/05-*.yaml "${K8S_DIR}"/06-*.yaml "${K8S_DIR}"/07-*.yaml; do
    if [[ -f "$manifest" ]]; then
        sed -i "s|ECR_REGISTRY|${ECR_REGISTRY}|g" "$manifest" 2>/dev/null || true
        sed -i "s|RESOURCE_PREFIX|${RESOURCE_PREFIX}|g" "$manifest" 2>/dev/null || true
        sed -i "s|IMAGE_TAG|${IMAGE_TAG}|g" "$manifest" 2>/dev/null || true
    fi
done

echo -e "${GREEN}✓${NC} Manifests updated"
echo ""

#------------------------------------------------------------------------------
# Apply manifests in order
#------------------------------------------------------------------------------

echo "Applying Kubernetes manifests..."
echo ""

# Order matters: namespaces first, then resources that depend on them
MANIFESTS=(
    "00-namespaces.yaml"
    "01-storage-class.yaml"
    "02-pvc.yaml"
    "03-secrets.yaml"
    "04-rbac.yaml"
    "05-gateway.yaml"
    "06-control.yaml"
    "07-scheduler.yaml"
    "08-network-policies.yaml"
    "09-runtime-class.yaml"
)

for manifest in "${MANIFESTS[@]}"; do
    manifest_path="${K8S_DIR}/${manifest}"
    
    # Use temp file for secrets (contains injected values)
    if [[ "$manifest" == "03-secrets.yaml" ]]; then
        manifest_path="${SECRETS_YAML_TMP}"
    fi
    
    if [[ -f "$manifest_path" ]]; then
        echo "Applying ${manifest}..."
        kubectl apply -f "$manifest_path"
        echo -e "${GREEN}✓${NC} Applied ${manifest}"
    else
        echo -e "${YELLOW}⚠${NC} Skipping ${manifest} (not found)"
    fi
done

# Clean up temp secrets file (don't leave secrets on disk)
if [[ -f "$SECRETS_YAML_TMP" ]]; then
    rm -f "$SECRETS_YAML_TMP"
fi

echo ""

#------------------------------------------------------------------------------
# Wait for deployments
#------------------------------------------------------------------------------

echo "Waiting for deployments to be ready..."
echo ""

kubectl rollout status deployment/aura-swarm-gateway -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s || true
kubectl rollout status deployment/aura-swarm-control -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s || true
kubectl rollout status deployment/aura-swarm-scheduler -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s || true

echo ""
echo -e "${GREEN}=============================================="
echo "  Kubernetes resources deployed!"
echo "==============================================${NC}"
echo ""

echo "Pods in ${K8S_NAMESPACE_SYSTEM}:"
kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}"

echo ""
echo "Services:"
kubectl get svc -n "${K8S_NAMESPACE_SYSTEM}"

echo ""
echo "Next step: Run ./09-verify.sh"
