#!/bin/bash
# 02-coco-operator.sh - Install the Confidential Containers operator, the
# CcRuntime CR (kata payload install on SNP nodes) and the RuntimeClasses.
#
# Verifies: operator controller ready, kata-qemu-snp RuntimeClass exists,
# every SNP node reports the kata runtime installed
# (katacontainers.io/kata-runtime=true).
#
# Usage: ./02-coco-operator.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "02" "CoCo operator + CcRuntime + RuntimeClasses"

require_cmds aws kubectl jq
require_aws_auth
ensure_kubectl_context

# Pinned operator release (same pin as the original install).
COCO_OPERATOR_VERSION="${COCO_OPERATOR_VERSION:-v0.17.0}"

#------------------------------------------------------------------------------
# Operator (idempotent kustomize apply)
#------------------------------------------------------------------------------

echo "Installing Confidential Containers operator (${COCO_OPERATOR_VERSION})..."
kubectl apply -k "github.com/confidential-containers/operator/config/default?ref=${COCO_OPERATOR_VERSION}"

kubectl rollout status deployment/cc-operator-controller-manager \
    -n confidential-containers-system --timeout=300s \
    || step_fail "CoCo operator controller did not become ready"
echo -e "${GREEN}✓${NC} CoCo operator controller ready"
echo ""

#------------------------------------------------------------------------------
# RuntimeClasses + CcRuntime
#------------------------------------------------------------------------------

echo "Applying RuntimeClasses (kata-qemu, kata-qemu-snp)..."
kubectl apply -f "${SCRIPT_DIR}/k8s/09-runtime-class.yaml"

echo "Applying CcRuntime CR (kata install on SNP nodes)..."
kubectl apply -f "${SCRIPT_DIR}/k8s/10-coco-ccruntime.yaml"
echo ""

#------------------------------------------------------------------------------
# Verify: RuntimeClass exists
#------------------------------------------------------------------------------

kubectl get runtimeclass kata-qemu-snp >/dev/null 2>&1 \
    || step_fail "RuntimeClass kata-qemu-snp does not exist after apply"
echo -e "${GREEN}✓${NC} RuntimeClass kata-qemu-snp exists"

#------------------------------------------------------------------------------
# Verify: kata runtime installed on every SNP node (CcRuntime install
# daemonsets label nodes katacontainers.io/kata-runtime=true when done)
#------------------------------------------------------------------------------

SNP_TOTAL=$(kubectl get nodes -l swarm.io/confidential-node=true --no-headers 2>/dev/null | wc -l | tr -d ' ')
if [[ "${SNP_TOTAL}" -eq 0 ]]; then
    step_fail "no SNP nodes found (label swarm.io/confidential-node=true) — run ./01-snp-node-group.sh first"
fi

TIMEOUT="${KATA_INSTALL_TIMEOUT_SECS:-1200}"
POLL=20
ELAPSED=0
INSTALLED=0

echo "Waiting for the kata runtime install on ${SNP_TOTAL} SNP node(s) (timeout ${TIMEOUT}s)..."
while [[ ${ELAPSED} -le ${TIMEOUT} ]]; do
    INSTALLED=$(kubectl get nodes \
        -l 'swarm.io/confidential-node=true,katacontainers.io/kata-runtime=true' \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')
    if [[ "${INSTALLED}" -ge "${SNP_TOTAL}" ]]; then
        break
    fi
    CC_STATUS=$(kubectl get ccruntime ccruntime -o jsonpath='{.status.runtimeClass}' 2>/dev/null || echo "")
    echo "  [${ELAPSED}s] kata installed: ${INSTALLED}/${SNP_TOTAL} node(s)  ccruntime=${CC_STATUS:-pending}"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

if [[ "${INSTALLED}" -lt "${SNP_TOTAL}" ]]; then
    echo "Diagnostics:"
    kubectl get ccruntime ccruntime -o jsonpath='{.status}' 2>/dev/null | jq . 2>/dev/null || true
    kubectl get pods -n confidential-containers-system -o wide || true
    step_fail "kata runtime installed on only ${INSTALLED}/${SNP_TOTAL} SNP node(s) after ${TIMEOUT}s"
fi
echo -e "${GREEN}✓${NC} kata runtime installed on all ${SNP_TOTAL} SNP node(s)"

step_ok "03 (./03-trustee-kbs.sh)"
