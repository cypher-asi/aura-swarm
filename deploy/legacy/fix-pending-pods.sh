#!/bin/bash
# fix-pending-pods.sh - Fix EFS CSI IRSA trust policy and unblock Pending pods
#
# Root cause: The EFS CSI controller's service account cannot AssumeRoleWithWebIdentity
# because the IAM trust policy on the EFS CSI role doesn't match the cluster's OIDC
# provider or the service account identity. This script rebuilds the trust policy
# from the live cluster state, then deletes/recreates PVCs to trigger reprovisioning.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

echo "=============================================="
echo "  Fix: EFS CSI IRSA Trust Policy"
echo "=============================================="
echo ""

#------------------------------------------------------------------------------
# 1. Gather live values from cluster + AWS
#------------------------------------------------------------------------------

echo -e "${CYAN}[1/6] Gathering cluster identity...${NC}"

AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
echo "  AWS Account:  ${AWS_ACCOUNT_ID}"

# Get OIDC issuer URL directly from the EKS cluster (single source of truth)
OIDC_ISSUER_URL=$(aws eks describe-cluster \
    --name "${EKS_CLUSTER_NAME}" \
    --query "cluster.identity.oidc.issuer" \
    --output text)
# Strip https:// for use in trust policy conditions
OIDC_PROVIDER="${OIDC_ISSUER_URL#https://}"
echo "  OIDC issuer:  ${OIDC_ISSUER_URL}"
echo "  OIDC provider (stripped): ${OIDC_PROVIDER}"

# Find the actual service account used by the EFS CSI controller
CSI_SA=$(kubectl get deployment efs-csi-controller -n kube-system \
    -o jsonpath='{.spec.template.spec.serviceAccountName}' 2>/dev/null || echo "efs-csi-controller-sa")
echo "  CSI controller SA: ${CSI_SA}"

EFS_ROLE_NAME="${RESOURCE_PREFIX}-efs-csi-role"
EFS_ROLE_ARN="arn:aws:iam::${AWS_ACCOUNT_ID}:role/${EFS_ROLE_NAME}"
echo "  Target IAM role: ${EFS_ROLE_NAME}"
echo ""

#------------------------------------------------------------------------------
# 2. Show current trust policy for comparison
#------------------------------------------------------------------------------

echo -e "${CYAN}[2/6] Checking current trust policy...${NC}"

CURRENT_POLICY=$(aws iam get-role --role-name "${EFS_ROLE_NAME}" \
    --query "Role.AssumeRolePolicyDocument" --output json 2>/dev/null || echo "{}")

# Extract the sub condition from existing policy to show what's wrong
CURRENT_SUB=$(echo "$CURRENT_POLICY" | jq -r '
    .Statement[0].Condition
    | to_entries[]
    | .value
    | to_entries[]
    | select(.key | endswith(":sub"))
    | .value' 2>/dev/null || echo "<could not parse>")
CURRENT_FEDERATED=$(echo "$CURRENT_POLICY" | jq -r '.Statement[0].Principal.Federated // "<missing>"' 2>/dev/null || echo "<could not parse>")

echo "  Current Federated principal:"
echo "    ${CURRENT_FEDERATED}"
echo "  Current :sub condition:"
echo "    ${CURRENT_SUB}"
echo ""

EXPECTED_FEDERATED="arn:aws:iam::${AWS_ACCOUNT_ID}:oidc-provider/${OIDC_PROVIDER}"
EXPECTED_SUB="system:serviceaccount:kube-system:${CSI_SA}"

echo "  Expected Federated principal:"
echo "    ${EXPECTED_FEDERATED}"
echo "  Expected :sub condition:"
echo "    ${EXPECTED_SUB}"
echo ""

#------------------------------------------------------------------------------
# 3. Rebuild trust policy with correct values
#------------------------------------------------------------------------------

echo -e "${CYAN}[3/6] Updating IAM trust policy...${NC}"

NEW_TRUST_POLICY=$(cat <<EOF
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

aws iam update-assume-role-policy \
    --role-name "${EFS_ROLE_NAME}" \
    --policy-document "$NEW_TRUST_POLICY"

echo -e "${GREEN}✓${NC} Trust policy updated"
echo ""

# Verify the policy has the AmazonEFSCSIDriverPolicy attached
ATTACHED=$(aws iam list-attached-role-policies --role-name "${EFS_ROLE_NAME}" \
    --query "AttachedPolicies[?PolicyName=='AmazonEFSCSIDriverPolicy'].PolicyName" \
    --output text 2>/dev/null || echo "")

if [[ -z "$ATTACHED" ]]; then
    echo "  Attaching AmazonEFSCSIDriverPolicy..."
    aws iam attach-role-policy \
        --role-name "${EFS_ROLE_NAME}" \
        --policy-arn arn:aws:iam::aws:policy/service-role/AmazonEFSCSIDriverPolicy
    echo -e "  ${GREEN}✓${NC} Policy attached"
else
    echo -e "  ${GREEN}✓${NC} AmazonEFSCSIDriverPolicy already attached"
fi
echo ""

#------------------------------------------------------------------------------
# 4. Verify the EKS add-on is linked to the role, then restart CSI driver
#------------------------------------------------------------------------------

echo -e "${CYAN}[4/6] Updating EFS CSI add-on and restarting...${NC}"

# Make sure the EKS add-on points to the correct role
ADDON_ROLE=$(aws eks describe-addon \
    --cluster-name "${EKS_CLUSTER_NAME}" \
    --addon-name aws-efs-csi-driver \
    --query "addon.serviceAccountRoleArn" \
    --output text 2>/dev/null || echo "None")

echo "  Add-on serviceAccountRoleArn: ${ADDON_ROLE}"

if [[ "$ADDON_ROLE" != "$EFS_ROLE_ARN" ]]; then
    echo "  Updating add-on to use role ${EFS_ROLE_ARN}..."
    aws eks update-addon \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --addon-name aws-efs-csi-driver \
        --service-account-role-arn "${EFS_ROLE_ARN}" \
        --resolve-conflicts OVERWRITE
    echo "  Waiting for add-on update..."
    aws eks wait addon-active \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --addon-name aws-efs-csi-driver
    echo -e "  ${GREEN}✓${NC} Add-on updated"
else
    echo -e "  ${GREEN}✓${NC} Add-on already configured with correct role"
fi

# Restart CSI controller so it picks up new credentials
echo "  Restarting EFS CSI controller pods..."
kubectl rollout restart deployment efs-csi-controller -n kube-system
kubectl rollout status deployment efs-csi-controller -n kube-system --timeout=120s
echo -e "${GREEN}✓${NC} EFS CSI controller restarted"
echo ""

#------------------------------------------------------------------------------
# 5. Delete stuck PVCs so they get reprovisioned
#------------------------------------------------------------------------------

echo -e "${CYAN}[5/6] Reprovisioning PVCs...${NC}"

delete_and_recreate_pvc() {
    local pvc_name="$1"
    local namespace="$2"
    local storage_size="$3"

    local status
    status=$(kubectl get pvc "$pvc_name" -n "$namespace" -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")

    if [[ "$status" == "Bound" ]]; then
        echo -e "  ${GREEN}✓${NC} ${pvc_name}: already Bound"
        return
    fi

    echo "  Deleting ${pvc_name} (${status})..."
    kubectl delete pvc "$pvc_name" -n "$namespace" --wait=false 2>/dev/null || true
    # Wait for deletion to complete
    kubectl wait --for=delete pvc/"$pvc_name" -n "$namespace" --timeout=30s 2>/dev/null || true

    echo "  Recreating ${pvc_name}..."
    cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: ${pvc_name}
  namespace: ${namespace}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: efs-sc
  resources:
    requests:
      storage: ${storage_size}
EOF
    echo -e "  ${GREEN}✓${NC} ${pvc_name} recreated"
}

delete_and_recreate_pvc "aura-swarm-gateway-data" "${K8S_NAMESPACE_SYSTEM}" "10Gi"
delete_and_recreate_pvc "aura-swarm-control-data" "${K8S_NAMESPACE_SYSTEM}" "10Gi"
delete_and_recreate_pvc "swarm-agent-state" "${K8S_NAMESPACE_AGENTS}" "100Gi"

# Give the CSI driver a moment to process the new PVCs
echo ""
echo "  Waiting 15s for PVC provisioning..."
sleep 15

# Check PVC status
echo ""
for pvc_ns in "aura-swarm-gateway-data:${K8S_NAMESPACE_SYSTEM}" \
              "aura-swarm-control-data:${K8S_NAMESPACE_SYSTEM}" \
              "swarm-agent-state:${K8S_NAMESPACE_AGENTS}"; do
    pvc="${pvc_ns%%:*}"
    ns="${pvc_ns##*:}"
    status=$(kubectl get pvc "$pvc" -n "$ns" -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")
    if [[ "$status" == "Bound" ]]; then
        echo -e "  ${GREEN}✓${NC} ${pvc}: Bound"
    else
        echo -e "  ${YELLOW}⚠${NC} ${pvc}: ${status}"
        kubectl get events -n "$ns" --field-selector "involvedObject.name=${pvc}" \
            --sort-by='.lastTimestamp' 2>/dev/null | tail -3 | while read -r line; do
            echo "      $line"
        done
    fi
done
echo ""

#------------------------------------------------------------------------------
# 6. Restart deployments and wait
#------------------------------------------------------------------------------

echo -e "${CYAN}[6/6] Restarting deployments and waiting...${NC}"

kubectl rollout restart deployment/aura-swarm-gateway -n "${K8S_NAMESPACE_SYSTEM}"
kubectl rollout restart deployment/aura-swarm-control -n "${K8S_NAMESPACE_SYSTEM}"

TIMEOUT=180
INTERVAL=10
ELAPSED=0

while [[ $ELAPSED -lt $TIMEOUT ]]; do
    GW=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -l app=aura-swarm-gateway \
        -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "?")
    CTL=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -l app=aura-swarm-control \
        -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "?")

    echo "  [${ELAPSED}s] gateway=${GW}  control=${CTL}"

    if [[ "$GW" == "Running" && "$CTL" == "Running" ]]; then
        break
    fi
    sleep "$INTERVAL"
    ELAPSED=$((ELAPSED + INTERVAL))
done

echo ""
echo "=============================================="
echo "  Final Status"
echo "=============================================="
echo ""

kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -o wide
echo ""
kubectl get pvc -n "${K8S_NAMESPACE_SYSTEM}"
echo ""
kubectl get pvc -n "${K8S_NAMESPACE_AGENTS}"
echo ""

GW_PHASE=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -l app=aura-swarm-gateway \
    -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "?")
CTL_PHASE=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -l app=aura-swarm-control \
    -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "?")

if [[ "$GW_PHASE" == "Running" && "$CTL_PHASE" == "Running" ]]; then
    echo -e "${GREEN}All pods are running! Run ./09-verify.sh for full health check.${NC}"
else
    echo -e "${RED}Pods still not running.${NC}"
    echo ""
    echo "Debug commands:"
    echo "  kubectl describe pvc aura-swarm-gateway-data -n ${K8S_NAMESPACE_SYSTEM}"
    echo "  kubectl logs -n kube-system -l app=efs-csi-controller --tail=30"
    echo "  kubectl describe pod -l app=aura-swarm-gateway -n ${K8S_NAMESPACE_SYSTEM}"
    exit 1
fi
