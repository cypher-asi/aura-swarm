#!/bin/bash
# 08-deploy-k8s.sh - Deploy Kubernetes manifests
#
# Deploys:
# - Namespaces (swarm-system, swarm-agents)
# - StorageClass, PVC
# - Secrets (placeholders), RBAC
# - Deployments (gateway, control, scheduler)
# - Network policies
#
# Usage:
#   ./08-deploy-k8s.sh              # Normal deploy (preserves data)
#   ./08-deploy-k8s.sh --reset-data # Fresh deploy (wipes databases)

set -euo pipefail

#------------------------------------------------------------------------------
# Parse arguments
#------------------------------------------------------------------------------

RESET_DATA=false
RECREATE_AGENTS=true

for arg in "$@"; do
    case $arg in
        --reset-data)
            RESET_DATA=true
            ;;
        --recreate-agents)
            RECREATE_AGENTS=true
            ;;
        --no-recreate-agents)
            RECREATE_AGENTS=false
            ;;
        --help|-h)
            echo "Usage: $0 [--reset-data] [--recreate-agents|--no-recreate-agents]"
            echo ""
            echo "Options:"
            echo "  --reset-data  Delete gateway and control plane databases before deploy"
            echo "                (wipes all agent records, sessions, and user data)"
            echo "  --recreate-agents     Recreate running swarm-agent pods after deploy (default)"
            echo "  --no-recreate-agents  Skip post-deploy swarm-agent convergence"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

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
echo "  Recreate running agents after deploy: ${RECREATE_AGENTS}"
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
Z_BILLING_API_KEY=$(load_secret "Z_BILLING_API_KEY")

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

# Copy manifests to a temp directory so we never modify the tracked originals
DEPLOY_TMP_DIR=$(mktemp -d)
trap "rm -rf '${DEPLOY_TMP_DIR}'" EXIT
cp "${K8S_DIR}"/*.yaml "${DEPLOY_TMP_DIR}/"

# Update storage class with EFS ID
sed -i "s/EFS_FILESYSTEM_ID/${EFS_ID}/g" "${DEPLOY_TMP_DIR}/01-storage-class.yaml" 2>/dev/null || true

# Update ConfigMap with image URLs
RUNTIME_IMAGE="${ECR_REGISTRY}/${RESOURCE_PREFIX}-runtime:${IMAGE_TAG}"
HARNESS_IMAGE="${ECR_REGISTRY}/${RESOURCE_PREFIX}-harness:${IMAGE_TAG}"
HARNESS_STATE_FILE="${SCRIPT_DIR}/.last-harness-image.env"
PINNED_HARNESS_IMAGE="${AURA_HARNESS_IMAGE:-}"

if [[ -z "${PINNED_HARNESS_IMAGE}" && -f "${HARNESS_STATE_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${HARNESS_STATE_FILE}"
    PINNED_HARNESS_IMAGE="${AURA_HARNESS_IMAGE:-}"
fi

if [[ -z "${PINNED_HARNESS_IMAGE}" ]]; then
    echo "Resolving immutable harness digest for deploy..."
    HARNESS_DIGEST=$(aws ecr describe-images \
        --repository-name "${RESOURCE_PREFIX}-harness" \
        --image-ids imageTag="${IMAGE_TAG}" \
        --query 'imageDetails[0].imageDigest' \
        --output text 2>/dev/null || echo "")
    if [[ -n "${HARNESS_DIGEST}" && "${HARNESS_DIGEST}" != "None" ]]; then
        PINNED_HARNESS_IMAGE="${ECR_REGISTRY}/${RESOURCE_PREFIX}-harness@${HARNESS_DIGEST}"
    fi
fi

if [[ -z "${PINNED_HARNESS_IMAGE}" ]]; then
    echo -e "${YELLOW}⚠${NC} Could not resolve harness digest; using mutable tag reference"
    PINNED_HARNESS_IMAGE="${HARNESS_IMAGE}"
else
    echo -e "${GREEN}✓${NC} Using pinned harness image: ${PINNED_HARNESS_IMAGE}"
fi

sed -i "s|REPLACE_WITH_ECR_REGISTRY/RESOURCE_PREFIX-runtime:v0.1.0|${RUNTIME_IMAGE}|g" "${DEPLOY_TMP_DIR}/03-secrets.yaml" 2>/dev/null || true
sed -i "s|ECR_REGISTRY/RESOURCE_PREFIX-harness:IMAGE_TAG|${PINNED_HARNESS_IMAGE}|g" "${DEPLOY_TMP_DIR}/03-secrets.yaml" 2>/dev/null || true

# Inject secrets into the secrets manifest (use a temp file to avoid partial writes)
SECRETS_YAML="${DEPLOY_TMP_DIR}/03-secrets.yaml"
SECRETS_YAML_TMP="${DEPLOY_TMP_DIR}/03-secrets.yaml.tmp"

cp "$SECRETS_YAML" "$SECRETS_YAML_TMP"
sed -i "s|__ANTHROPIC_API_KEY__|${ANTHROPIC_API_KEY}|g" "$SECRETS_YAML_TMP"
sed -i "s|__OPENAI_API_KEY__|${OPENAI_API_KEY:-placeholder-not-set}|g" "$SECRETS_YAML_TMP"
sed -i "s|__ZERO_ID_SECRET__|${ZERO_ID_SECRET:-placeholder-not-set}|g" "$SECRETS_YAML_TMP"
sed -i "s|__Z_BILLING_API_KEY__|${Z_BILLING_API_KEY:-}|g" "$SECRETS_YAML_TMP"
sed -i "s|__DEFAULT_ISOLATION__|${DEFAULT_ISOLATION}|g" "$SECRETS_YAML_TMP"

# Update deployments with ECR image URLs
for manifest in "${DEPLOY_TMP_DIR}"/05-*.yaml "${DEPLOY_TMP_DIR}"/06-*.yaml "${DEPLOY_TMP_DIR}"/07-*.yaml; do
    if [[ -f "$manifest" ]]; then
        sed -i "s|ECR_REGISTRY|${ECR_REGISTRY}|g" "$manifest" 2>/dev/null || true
        sed -i "s|RESOURCE_PREFIX|${RESOURCE_PREFIX}|g" "$manifest" 2>/dev/null || true
        sed -i "s|IMAGE_TAG|${IMAGE_TAG}|g" "$manifest" 2>/dev/null || true
    fi
done

echo -e "${GREEN}✓${NC} Manifests updated"
echo ""

#------------------------------------------------------------------------------
# Reset data (if requested)
#------------------------------------------------------------------------------

if [[ "$RESET_DATA" == "true" ]]; then
    echo -e "${RED}WARNING: --reset-data flag detected${NC}"
    echo ""
    echo "This will delete ALL stored data including:"
    echo "  - Agent records"
    echo "  - Session history"
    echo "  - User data"
    echo ""
    read -p "Are you sure you want to continue? [y/N] " -n 1 -r
    echo ""
    
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "Aborted."
        exit 1
    fi
    
    echo ""
    echo "Deleting data PVCs..."
    
    # Delete gateway data PVC
    if kubectl get pvc aura-swarm-gateway-data -n "${K8S_NAMESPACE_SYSTEM}" &>/dev/null; then
        echo "  Deleting aura-swarm-gateway-data..."
        kubectl delete pvc aura-swarm-gateway-data -n "${K8S_NAMESPACE_SYSTEM}" --wait=true
        echo -e "  ${GREEN}✓${NC} Deleted gateway data"
    else
        echo -e "  ${YELLOW}⚠${NC} aura-swarm-gateway-data not found (skipping)"
    fi
    
    # Delete control plane data PVC
    if kubectl get pvc aura-swarm-control-data -n "${K8S_NAMESPACE_SYSTEM}" &>/dev/null; then
        echo "  Deleting aura-swarm-control-data..."
        kubectl delete pvc aura-swarm-control-data -n "${K8S_NAMESPACE_SYSTEM}" --wait=true
        echo -e "  ${GREEN}✓${NC} Deleted control plane data"
    else
        echo -e "  ${YELLOW}⚠${NC} aura-swarm-control-data not found (skipping)"
    fi
    
    echo ""
    echo -e "${GREEN}✓${NC} Data reset complete. PVCs will be recreated during deploy."
    echo ""
fi

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
    manifest_path="${DEPLOY_TMP_DIR}/${manifest}"
    
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

# Clean up temp directory (don't leave secrets on disk)
rm -rf "${DEPLOY_TMP_DIR}"

echo ""

#------------------------------------------------------------------------------
# Pre-flight: verify EFS CSI driver + PVCs before waiting on deployments
#------------------------------------------------------------------------------

echo "Checking EFS CSI driver health..."

EFS_CSI_RUNNING=$(kubectl get pods -n kube-system -l app=efs-csi-controller \
    --no-headers 2>/dev/null | grep -c "Running" || true)

if [[ "$EFS_CSI_RUNNING" -eq 0 ]]; then
    echo -e "${RED}✗${NC} EFS CSI controller is not running!"
    echo "  The gateway and control PVCs will not provision without it."
    echo "  Run ./05-configure-eks.sh to install the EFS CSI driver, then re-run this script."
    echo ""
    echo "  Quick check:  kubectl get pods -n kube-system -l app=efs-csi-controller"
    exit 1
fi

echo -e "${GREEN}✓${NC} EFS CSI controller is running"
echo ""

echo "Waiting for PVCs to bind (up to 60s)..."

PVC_TIMEOUT=60
PVC_INTERVAL=5
PVC_ELAPSED=0
PVC_OK=false

while [[ $PVC_ELAPSED -lt $PVC_TIMEOUT ]]; do
    GW_PVC=$(kubectl get pvc aura-swarm-gateway-data -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")
    CTL_PVC=$(kubectl get pvc aura-swarm-control-data -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")

    echo "  [${PVC_ELAPSED}s] gateway-data=${GW_PVC}  control-data=${CTL_PVC}"

    if [[ "$GW_PVC" == "Bound" && "$CTL_PVC" == "Bound" ]]; then
        PVC_OK=true
        break
    fi
    sleep "$PVC_INTERVAL"
    PVC_ELAPSED=$((PVC_ELAPSED + PVC_INTERVAL))
done

echo ""

if [[ "$PVC_OK" == "true" ]]; then
    echo -e "${GREEN}✓${NC} All PVCs bound"
else
    echo -e "${RED}✗${NC} PVCs did not bind within ${PVC_TIMEOUT}s"
    echo ""

    for PVC_NAME in aura-swarm-gateway-data aura-swarm-control-data; do
        PVC_PHASE=$(kubectl get pvc "$PVC_NAME" -n "${K8S_NAMESPACE_SYSTEM}" \
            -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")
        if [[ "$PVC_PHASE" != "Bound" ]]; then
            echo "  --- ${PVC_NAME} (${PVC_PHASE}) ---"
            kubectl get events -n "${K8S_NAMESPACE_SYSTEM}" \
                --field-selector "involvedObject.name=${PVC_NAME}" \
                --sort-by='.lastTimestamp' 2>/dev/null | tail -5
            echo ""
        fi
    done

    echo -e "${YELLOW}Common causes:${NC}"
    echo "  1. IRSA misconfigured — trust policy doesn't match cluster OIDC provider"
    echo "     Fix: re-run ./05-configure-eks.sh (it reconciles the trust policy)"
    echo "  2. EFS CSI driver unhealthy"
    echo "     Check: kubectl logs -n kube-system -l app=efs-csi-controller --tail=20"
    echo "  3. EFS mount targets not reachable from node subnets"
    echo "     Check: security groups allow NFS (2049) from agent subnets to EFS"
    echo ""
    echo "After fixing, re-run this script."
    exit 1
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

#------------------------------------------------------------------------------
# Recreate running agent pods to converge to pinned harness digest (default on)
#------------------------------------------------------------------------------

if [[ "${RECREATE_AGENTS}" == "true" ]]; then
    echo "Converging running swarm-agent pods by recreation..."
    mapfile -t RUNNING_AGENT_PODS < <(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent \
        --field-selector=status.phase=Running \
        -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null || true)

    if [[ ${#RUNNING_AGENT_PODS[@]} -eq 0 ]]; then
        echo "No running swarm-agent pods found; nothing to recreate."
    else
        echo "Recreating ${#RUNNING_AGENT_PODS[@]} running agent pod(s)..."
        kubectl delete pod -n "${K8S_NAMESPACE_AGENTS}" "${RUNNING_AGENT_PODS[@]}" --wait=false

        echo "Waiting for replacement swarm-agent pods to appear..."
        AGENT_APPEAR_TIMEOUT=60
        AGENT_APPEAR_ELAPSED=0
        AGENT_APPEARED=false
        while [[ "${AGENT_APPEAR_ELAPSED}" -lt "${AGENT_APPEAR_TIMEOUT}" ]]; do
            AGENT_POD_COUNT=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent \
                --no-headers 2>/dev/null | wc -l | tr -d ' ' || echo "0")
            if [[ "${AGENT_POD_COUNT}" -gt 0 ]]; then
                AGENT_APPEARED=true
                break
            fi
            sleep 5
            AGENT_APPEAR_ELAPSED=$((AGENT_APPEAR_ELAPSED + 5))
        done

        if [[ "${AGENT_APPEARED}" != "true" ]]; then
            echo -e "${RED}✗${NC} Agent convergence failed: no replacement pods appeared."
            echo "Rollback guidance:"
            echo "  - Inspect controller state: kubectl get pods -n ${K8S_NAMESPACE_SYSTEM} -o wide"
            echo "  - Check scheduler logs:     kubectl logs -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-scheduler --tail=50"
            echo "  - Re-run deploy with explicit convergence: ./08-deploy-k8s.sh --recreate-agents"
            exit 1
        fi

        echo "Waiting for replacement swarm-agent pods to become Ready (timeout 300s)..."
        if ! kubectl wait --for=condition=Ready pod -l app=swarm-agent \
            -n "${K8S_NAMESPACE_AGENTS}" --timeout=300s >/dev/null; then
            echo -e "${RED}✗${NC} Agent convergence failed: replacement pods did not become Ready in time."
            echo "Rollback guidance:"
            echo "  - Inspect agent pod status: kubectl get pods -n ${K8S_NAMESPACE_AGENTS} -l app=swarm-agent -o wide"
            echo "  - Check recent events:      kubectl get events -n ${K8S_NAMESPACE_AGENTS} --sort-by='.lastTimestamp' | tail -20"
            echo "  - Re-run deploy with explicit convergence: ./08-deploy-k8s.sh --recreate-agents"
            exit 1
        fi
        echo -e "${GREEN}✓${NC} Agent pods recreated and Ready"
    fi
else
    echo -e "${YELLOW}⚠${NC} Skipping post-deploy agent recreation (--no-recreate-agents)"
fi

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
