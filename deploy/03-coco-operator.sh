#!/bin/bash
# 03-coco-operator.sh - Install the confidential runtime (Cloud API Adaptor).
#
# Confidential agents run as Peer Pods: the Cloud API Adaptor (CAA) Helm release
# (pinned CAA_CHART_VERSION) installs the kata-remote RuntimeClass and boots a
# per-agent AWS-managed SEV-SNP pod VM off-cluster. Workers stay ordinary
# instances (no per-node kata payload, no SNP metal pool).
# Verifies: CAA daemonset Ready on the workers, kata-remote RuntimeClass exists.
#
# Usage: ./03-coco-operator.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "03" "Confidential runtime (Cloud API Adaptor / Peer Pods)"

require_cmds helm aws kubectl jq
require_aws_auth
ensure_kubectl_context

# CAA cannot launch a confidential pod VM without a pod-VM AMI.
PODVM_AMI_ID="${PODVM_AMI_ID:-}"
[[ -n "${PODVM_AMI_ID}" ]] \
    || step_fail "PODVM_AMI_ID is empty — set it in config.env or auto-discover with ./configure.sh (see deploy/PEER-PODS-PLAN.md §3)"

PODVM_INSTANCE_TYPE="${PODVM_INSTANCE_TYPE:-m6a.large}"
CAA_VXLAN_PORT="${CAA_VXLAN_PORT:-9000}"
CAA_FORWARDER_PORT="${CAA_FORWARDER_PORT:-15150}"
CAA_CHART_VERSION="${CAA_CHART_VERSION:-}"
[[ -n "${CAA_CHART_VERSION}" ]] \
    || step_fail "CAA_CHART_VERSION is empty — pin the Cloud API Adaptor chart version in config.env before installing"

# CAA install targets (kept overridable; pinned defaults match the upstream chart).
CAA_NAMESPACE="${CAA_NAMESPACE:-confidential-containers-system}"
CAA_RELEASE="${CAA_RELEASE:-cloud-api-adaptor}"
CAA_DAEMONSET="${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}"
# TODO: confirm the chart repo/name + value keys against
# https://confidentialcontainers.org/docs/examples/aws-simple before first apply.
# The CoCo project publishes the cloud-api-adaptor chart as an OCI artifact in GHCR.
CAA_CHART_REF="${CAA_CHART_REF:-oci://ghcr.io/confidential-containers/cloud-api-adaptor/cloud-api-adaptor}"
CAA_INSTALL_TIMEOUT="${CAA_INSTALL_TIMEOUT_SECS:-600}"

# Terraform-provided network/IAM wiring.
AGENT_SUBNET_IDS_RAW="$(tf_output agent_subnet_ids)"
# agent_subnet_ids may be a TF list output; normalize to comma-separated.
if printf '%s' "${AGENT_SUBNET_IDS_RAW}" | jq -e 'type=="array"' >/dev/null 2>&1; then
    AGENT_SUBNET_IDS="$(printf '%s' "${AGENT_SUBNET_IDS_RAW}" | jq -r 'join(",")')"
else
    AGENT_SUBNET_IDS="${AGENT_SUBNET_IDS_RAW}"
fi
NODE_SG_ID="$(tf_output node_security_group_id)"
CAA_ROLE_ARN="$(tf_output caa_role_arn)"

[[ -n "${AGENT_SUBNET_IDS}" ]] \
    || step_fail "terraform output agent_subnet_ids is empty — apply the CAA infra (subnets/SG/IRSA) first (./02-snp-node-group.sh)"
[[ -n "${NODE_SG_ID}" ]] \
    || step_fail "terraform output node_security_group_id is empty — apply the CAA infra first (./02-snp-node-group.sh)"
if [[ -z "${CAA_ROLE_ARN}" ]]; then
    echo -e "${YELLOW}⚠${NC} terraform output caa_role_arn is empty — CAA will fall back to the node instance role."
    echo "  Prefer a dedicated IRSA role (PEER-PODS-PLAN §3 CAA IAM); set caa_role_arn in terraform."
fi

echo "Installing Cloud API Adaptor (chart ${CAA_CHART_REF} @ ${CAA_CHART_VERSION}) into ${CAA_NAMESPACE}..."
echo "  pod-VM AMI:        ${PODVM_AMI_ID}"
echo "  pod-VM type:       ${PODVM_INSTANCE_TYPE}"
echo "  region:            ${AWS_REGION}"
echo "  agent subnet(s):   ${AGENT_SUBNET_IDS}"
echo "  node SG:           ${NODE_SG_ID}"
echo "  vxlan/forwarder:   ${CAA_VXLAN_PORT} / ${CAA_FORWARDER_PORT}"
echo "  IRSA role:         ${CAA_ROLE_ARN:-<none — using node role>}"

kubectl create namespace "${CAA_NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# TODO: the --set value paths below mirror the aws-simple example's peer-pods
# config; confirm each key against the pinned chart's values.yaml
# (cloudProvider/aws.* and the service-account annotation path) before apply.
CAA_SET_ARGS=(
    --set "cloudProvider=aws"
    --set "aws.region=${AWS_REGION}"
    --set "aws.podvmAmiId=${PODVM_AMI_ID}"
    --set "aws.podvmInstanceType=${PODVM_INSTANCE_TYPE}"
    --set "aws.subnetIds=${AGENT_SUBNET_IDS}"
    --set "aws.securityGroupIds=${NODE_SG_ID}"
    --set "aws.disableCvm=false"
    --set "aws.vxlanPort=${CAA_VXLAN_PORT}"
    --set "aws.forwarderPort=${CAA_FORWARDER_PORT}"
)
if [[ -n "${CAA_ROLE_ARN}" ]]; then
    CAA_SET_ARGS+=(--set "serviceAccount.annotations.eks\.amazonaws\.com/role-arn=${CAA_ROLE_ARN}")
fi

if ! helm upgrade --install "${CAA_RELEASE}" "${CAA_CHART_REF}" \
    --version "${CAA_CHART_VERSION}" \
    --namespace "${CAA_NAMESPACE}" \
    "${CAA_SET_ARGS[@]}" \
    --wait --timeout "${CAA_INSTALL_TIMEOUT}s"; then
    echo ""
    echo -e "${YELLOW}--- Cloud API Adaptor diagnostics ---${NC}"
    kubectl get ds -n "${CAA_NAMESPACE}" -o wide 2>&1 | sed 's/^/  /' || true
    kubectl get pods -n "${CAA_NAMESPACE}" -o wide 2>&1 | sed 's/^/  /' || true
    CAA_POD=$(kubectl get pods -n "${CAA_NAMESPACE}" -l app=cloud-api-adaptor \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)
    if [[ -n "${CAA_POD}" ]]; then
        echo "  Events for ${CAA_POD}:"
        kubectl describe pod "${CAA_POD}" -n "${CAA_NAMESPACE}" 2>/dev/null | sed -n '/Events:/,$p' | sed 's/^/    /' || true
        echo "  Logs (tail) for ${CAA_POD}:"
        kubectl logs "${CAA_POD}" -n "${CAA_NAMESPACE}" --tail=50 2>&1 | sed 's/^/    /' || true
    fi
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
    step_fail "Cloud API Adaptor Helm install did not become ready within ${CAA_INSTALL_TIMEOUT}s"
fi
echo -e "${GREEN}✓${NC} Cloud API Adaptor Helm release ${CAA_RELEASE} installed"
echo ""

# Verify: CAA daemonset Ready on the workers.
DS_TIMEOUT="${CAA_INSTALL_TIMEOUT}"
POLL=15
ELAPSED=0
DS_READY=0
DS_DESIRED=0
echo "Waiting for the CAA daemonset (${CAA_DAEMONSET}) to be Ready on the workers (timeout ${DS_TIMEOUT}s)..."
while [[ ${ELAPSED} -le ${DS_TIMEOUT} ]]; do
    DS_DESIRED=$(kubectl get ds "${CAA_DAEMONSET}" -n "${CAA_NAMESPACE}" \
        -o jsonpath='{.status.desiredNumberScheduled}' 2>/dev/null || echo 0)
    DS_READY=$(kubectl get ds "${CAA_DAEMONSET}" -n "${CAA_NAMESPACE}" \
        -o jsonpath='{.status.numberReady}' 2>/dev/null || echo 0)
    if [[ "${DS_DESIRED:-0}" -ge 1 && "${DS_READY:-0}" -ge "${DS_DESIRED:-0}" ]]; then
        break
    fi
    echo "  [${ELAPSED}s] CAA daemonset Ready: ${DS_READY:-0}/${DS_DESIRED:-0}"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done
if [[ "${DS_DESIRED:-0}" -lt 1 || "${DS_READY:-0}" -lt "${DS_DESIRED:-0}" ]]; then
    step_fail "CAA daemonset ${CAA_DAEMONSET} not Ready (${DS_READY:-0}/${DS_DESIRED:-0}) after ${DS_TIMEOUT}s"
fi
echo -e "${GREEN}✓${NC} CAA daemonset Ready on ${DS_READY} worker(s)"

# Verify: the kata-remote RuntimeClass exists.
kubectl get runtimeclass kata-remote >/dev/null 2>&1 \
    || step_fail "RuntimeClass kata-remote does not exist after the CAA install"
echo -e "${GREEN}✓${NC} RuntimeClass kata-remote exists"

step_ok "04 (./04-trustee-kbs.sh)"
