#!/bin/bash
# 03-coco-operator.sh - Install the confidential runtime.
#
# CONFIDENTIAL_RUNTIME=snp_local (default/fallback): Confidential Containers
#   operator + CcRuntime CR (kata payload install on SNP nodes) + RuntimeClasses.
#   Verifies: operator controller ready, kata-qemu-snp RuntimeClass exists,
#   every SNP node reports katacontainers.io/kata-runtime=true.
# CONFIDENTIAL_RUNTIME=peer_pods: Cloud API Adaptor (CAA) Helm install
#   (pinned CAA_CHART_VERSION). CAA boots a per-pod AWS-managed SEV-SNP pod VM,
#   so workers stay ordinary instances and there is no per-node kata payload.
#   Verifies: CAA daemonset Ready on the workers, kata-remote RuntimeClass exists.
#
# Usage: ./03-coco-operator.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "03" "Confidential runtime (CoCo operator | Cloud API Adaptor)"

# snp_local = on-node kata-qemu-snp (default/fallback); peer_pods = CAA kata-remote.
CONFIDENTIAL_RUNTIME="${CONFIDENTIAL_RUNTIME:-snp_local}"

#------------------------------------------------------------------------------
# peer_pods: install the Cloud API Adaptor via Helm and verify kata-remote.
# Returns early (step_ok + exit) so the snp_local body below is untouched.
#------------------------------------------------------------------------------
if [[ "${CONFIDENTIAL_RUNTIME}" == "peer_pods" ]]; then
    require_cmds helm aws kubectl jq
    require_aws_auth
    ensure_kubectl_context

    # CAA cannot launch a confidential pod VM without a pod-VM AMI.
    PODVM_AMI_ID="${PODVM_AMI_ID:-}"
    [[ -n "${PODVM_AMI_ID}" ]] \
        || step_fail "PODVM_AMI_ID is empty — peer_pods needs a pod-VM AMI (set PODVM_AMI_ID in config.env; see deploy/SNP-AMI-PLAN.md / PEER-PODS-PLAN §3)"

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
    # TODO(phase4): confirm the chart repo/name + value keys against
    # https://confidentialcontainers.org/docs/examples/aws-simple before first apply.
    # The CoCo project publishes the cloud-api-adaptor chart as an OCI artifact in GHCR.
    CAA_CHART_REF="${CAA_CHART_REF:-oci://ghcr.io/confidential-containers/cloud-api-adaptor/cloud-api-adaptor}"
    CAA_INSTALL_TIMEOUT="${CAA_INSTALL_TIMEOUT_SECS:-600}"

    # Terraform-provided network/IAM wiring (added by the Phase 3 infra change).
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
        || step_fail "terraform output agent_subnet_ids is empty — apply the Phase 3 CAA infra (subnets/SG/IRSA) first"
    [[ -n "${NODE_SG_ID}" ]] \
        || step_fail "terraform output node_security_group_id is empty — apply the Phase 3 CAA infra first"
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

    # TODO(phase4): the --set value paths below mirror the aws-simple example's
    # peer-pods config; confirm each key against the pinned chart's values.yaml
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

    # Verify: the kata-remote RuntimeClass exists (replaces the kata-qemu-snp check).
    kubectl get runtimeclass kata-remote >/dev/null 2>&1 \
        || step_fail "RuntimeClass kata-remote does not exist after the CAA install"
    echo -e "${GREEN}✓${NC} RuntimeClass kata-remote exists"

    step_ok "04 (./04-trustee-kbs.sh)"
    exit 0
fi

#------------------------------------------------------------------------------
# snp_local (default/fallback): existing on-node CoCo install (unchanged).
#------------------------------------------------------------------------------

require_cmds aws kubectl jq
require_aws_auth
ensure_kubectl_context

# Make sure a confidential (SNP) node is actually Ready before installing the
# kata payload — auto-recovers a degraded node group and waits for the (slow,
# bare-metal) node to join, with live status. One command, no babysitting.
ensure_snp_node_ready
echo ""

# Pinned operator release (same pin as the original install).
COCO_OPERATOR_VERSION="${COCO_OPERATOR_VERSION:-v0.17.0}"

#------------------------------------------------------------------------------
# Operator (idempotent kustomize apply)
#------------------------------------------------------------------------------

CC_NS="confidential-containers-system"
CC_DEPLOY="cc-operator-controller-manager"
COCO_OPERATOR_TIMEOUT="${COCO_OPERATOR_TIMEOUT:-300}"
# The v0.17.0 controller-manager has no tolerations and requests 100m CPU; it
# can only land on an untainted, Ready node with that much CPU free.
CC_OPERATOR_CPU_REQ_M="${CC_OPERATOR_CPU_REQ_M:-100}"

# millicores of free CPU on the most-available untainted Ready node, or -1 if
# there is no such node. Echoes "unknown" if it cannot be computed.
max_untainted_cpu_headroom_m() {
    local nodes node alloc_m req_m free_m best=-1
    nodes=$(kubectl get nodes -o json 2>/dev/null | jq -r '
        .items[]
        | select((.spec.taints // []) | map(select(.effect == "NoSchedule")) | length == 0)
        | select((.status.conditions // []) | map(select(.type == "Ready" and .status == "True")) | length > 0)
        | .metadata.name' 2>/dev/null) || { echo "unknown"; return; }

    [[ -z "${nodes}" ]] && { echo "-1"; return; }

    while IFS= read -r node; do
        [[ -z "${node}" ]] && continue
        alloc_m=$(kubectl get node "${node}" -o json 2>/dev/null | jq '
            .status.allocatable.cpu
            | if test("m$") then (sub("m$"; "") | tonumber) else (tonumber * 1000) end' 2>/dev/null) || continue
        req_m=$(kubectl get pods -A --field-selector "spec.nodeName=${node},status.phase=Running" -o json 2>/dev/null | jq '
            [.items[].spec.containers[].resources.requests.cpu // "0"]
            | map(if test("m$") then (sub("m$"; "") | tonumber) else (tonumber * 1000) end)
            | add // 0' 2>/dev/null) || continue
        free_m=$(( alloc_m - req_m ))
        (( free_m > best )) && best=${free_m}
    done <<< "${nodes}"
    echo "${best}"
}

echo "Checking the system pool can schedule the operator (needs ${CC_OPERATOR_CPU_REQ_M}m CPU on an untainted Ready node)..."
HEADROOM_M=$(max_untainted_cpu_headroom_m)
if [[ "${HEADROOM_M}" == "unknown" ]]; then
    echo -e "${YELLOW}⚠${NC} Could not compute CPU headroom; proceeding (the rollout wait will catch scheduling issues)."
elif [[ "${HEADROOM_M}" == "-1" ]]; then
    echo -e "${RED}✗${NC} No untainted, Ready node is available for the operator."
    echo "  Scale the system node group up, wait for a Ready node, then re-run:"
    echo "    aws eks update-nodegroup-config --cluster-name ${EKS_CLUSTER_NAME} --nodegroup-name ${RESOURCE_PREFIX}-node-group --scaling-config minSize=${NODE_MIN_COUNT},maxSize=${NODE_MAX_COUNT},desiredSize=$((NODE_DESIRED_COUNT + 1)) --region ${AWS_REGION}"
    step_fail "no schedulable node for the CoCo operator"
elif (( HEADROOM_M < CC_OPERATOR_CPU_REQ_M )); then
    echo -e "${RED}✗${NC} System pool is CPU-saturated: max free on an untainted node is ${HEADROOM_M}m, operator needs ${CC_OPERATOR_CPU_REQ_M}m."
    echo "  Add a node to the system pool, wait for it to become Ready, then re-run:"
    echo "    aws eks update-nodegroup-config --cluster-name ${EKS_CLUSTER_NAME} --nodegroup-name ${RESOURCE_PREFIX}-node-group --scaling-config minSize=${NODE_MIN_COUNT},maxSize=${NODE_MAX_COUNT},desiredSize=$((NODE_DESIRED_COUNT + 1)) --region ${AWS_REGION}"
    step_fail "insufficient CPU headroom for the CoCo operator (free ${HEADROOM_M}m < needed ${CC_OPERATOR_CPU_REQ_M}m)"
else
    echo -e "${GREEN}✓${NC} Untainted node has ${HEADROOM_M}m CPU free (>= ${CC_OPERATOR_CPU_REQ_M}m)"
fi
echo ""

diagnose_operator() {
    echo ""
    echo -e "${YELLOW}--- CoCo operator diagnostics ---${NC}"
    echo "Deployment:"
    kubectl get deployment "${CC_DEPLOY}" -n "${CC_NS}" -o wide 2>&1 | sed 's/^/  /' || true
    echo "Pods:"
    kubectl get pods -n "${CC_NS}" -o wide 2>&1 | sed 's/^/  /' || true

    local pod
    pod=$(kubectl get pods -n "${CC_NS}" -l control-plane=controller-manager \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)
    if [[ -n "${pod}" ]]; then
        echo "Pod conditions / container state for ${pod}:"
        kubectl get pod "${pod}" -n "${CC_NS}" \
            -o jsonpath='{range .status.conditions[*]}  {.type}={.status} ({.reason} {.message}){"\n"}{end}' 2>/dev/null || true
        kubectl get pod "${pod}" -n "${CC_NS}" \
            -o jsonpath='{range .status.containerStatuses[*]}  container={.name} ready={.ready} waiting={.state.waiting.reason} {.state.waiting.message}{"\n"}{end}' 2>/dev/null || true
        echo "Recent pod events:"
        kubectl describe pod "${pod}" -n "${CC_NS}" 2>/dev/null \
            | sed -n '/Events:/,$p' | sed 's/^/  /' || true
    fi

    echo "Schedulable nodes (taints / readiness — the controller has NO tolerations, so it needs an untainted Ready node):"
    kubectl get nodes \
        -o custom-columns='NAME:.metadata.name,READY:.status.conditions[-1].type,ROLE:.metadata.labels.role,TAINTS:.spec.taints[*].key' \
        --no-headers 2>&1 | sed 's/^/  /' || true
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
}

echo "Installing Confidential Containers operator (${COCO_OPERATOR_VERSION})..."
kubectl apply -k "github.com/confidential-containers/operator/config/default?ref=${COCO_OPERATOR_VERSION}"

echo "Waiting for ${CC_DEPLOY} rollout (timeout ${COCO_OPERATOR_TIMEOUT}s)..."
if ! kubectl rollout status "deployment/${CC_DEPLOY}" \
    -n "${CC_NS}" --timeout="${COCO_OPERATOR_TIMEOUT}s"; then
    diagnose_operator
    echo ""
    echo "Common causes & fixes:"
    echo "  • Pod Pending / 'untolerated taint' or 'didn't match node selector':"
    echo "      no untainted Ready node. The system node group must have a node that is"
    echo "      NOT tainted swarm.io/confidential-node. Check the list above; if only the"
    echo "      SNP (tainted) node is Ready, scale the system node group up and re-run."
    echo "  • ImagePullBackOff: nodes can't reach quay.io — verify NAT egress from the"
    echo "      private subnets, then re-run (kubectl rollout restart deployment/${CC_DEPLOY} -n ${CC_NS})."
    echo "  • Slow first image pull: re-run this script, or raise COCO_OPERATOR_TIMEOUT=600."
    step_fail "CoCo operator controller did not become ready within ${COCO_OPERATOR_TIMEOUT}s (see diagnostics above)"
fi
echo -e "${GREEN}✓${NC} CoCo operator controller ready"
echo ""

#------------------------------------------------------------------------------
# RuntimeClasses + CcRuntime
#------------------------------------------------------------------------------

echo "Applying RuntimeClasses (kata-qemu, kata-qemu-snp)..."
kubectl apply -f "${SCRIPT_DIR}/k8s/09-runtime-class.yaml"

#------------------------------------------------------------------------------
# CcRuntime reconcile: a plain `kubectl apply` is a no-op when the spec text is
# unchanged AND the operator does not cleanly roll a changed payloadImage or a
# crashlooping install daemonset. So detect those and force a clean recreate.
#------------------------------------------------------------------------------
CCRUNTIME_MANIFEST="${SCRIPT_DIR}/k8s/10-coco-ccruntime.yaml"

ccruntime_clear() {
    echo "  Removing existing CcRuntime + any orphaned install daemonsets..."
    kubectl delete ccruntime ccruntime --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1 || true
    local tries=0
    while kubectl get ds -n "${CC_NS}" -o name 2>/dev/null | grep -q daemon; do
        kubectl delete ds -n "${CC_NS}" --all --ignore-not-found >/dev/null 2>&1 || true
        kubectl delete pods -n "${CC_NS}" --field-selector 'status.phase!=Running' \
            --force --grace-period=0 --ignore-not-found >/dev/null 2>&1 || true
        tries=$((tries + 1)); (( tries >= 6 )) && break
        sleep 5
    done
}

install_is_unhealthy() {
    kubectl get pods -n "${CC_NS}" -o json 2>/dev/null | jq -e '
        [ .items[]
          | select(.metadata.name | test("daemon-install"))
          | select(
              ((.status.containerStatuses // [])[]?.state.waiting.reason // "")
              | test("CrashLoopBackOff|Error|ImagePullBackOff|ErrImagePull")
            )
        ] | length > 0' >/dev/null 2>&1
}

node_already_kata() {
    [[ "$(kubectl get nodes -l 'swarm.io/confidential-node=true,katacontainers.io/kata-runtime=true' --no-headers 2>/dev/null | wc -l | tr -d ' ')" -ge 1 ]]
}

DESIRED_PAYLOAD=$(grep -E '^[[:space:]]*payloadImage:' "${CCRUNTIME_MANIFEST}" | awk '{print $2}')
CURRENT_PAYLOAD=$(kubectl get ccruntime ccruntime -o jsonpath='{.spec.config.payloadImage}' 2>/dev/null || echo "")

echo "Reconciling CcRuntime CR (kata install on SNP nodes)..."
echo "  desired payload: ${DESIRED_PAYLOAD}"
if [[ -n "${CURRENT_PAYLOAD}" ]]; then
    echo "  current payload: ${CURRENT_PAYLOAD}"
fi

if node_already_kata; then
    echo -e "  ${GREEN}✓${NC} kata runtime already installed on the SNP node(s); ensuring CR is applied."
    kubectl apply -f "${CCRUNTIME_MANIFEST}" >/dev/null
elif [[ -n "${CURRENT_PAYLOAD}" && "${CURRENT_PAYLOAD}" != "${DESIRED_PAYLOAD}" ]]; then
    echo "  payload changed — recreating CcRuntime for a clean install."
    ccruntime_clear
    kubectl apply -f "${CCRUNTIME_MANIFEST}" >/dev/null
elif [[ -n "${CURRENT_PAYLOAD}" ]] && install_is_unhealthy; then
    echo "  existing install daemonset is unhealthy (crash/imagepull) — recreating CcRuntime."
    ccruntime_clear
    kubectl apply -f "${CCRUNTIME_MANIFEST}" >/dev/null
else
    kubectl apply -f "${CCRUNTIME_MANIFEST}" >/dev/null
fi
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
    step_fail "no SNP nodes found (label swarm.io/confidential-node=true) — run ./02-snp-node-group.sh first"
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
    INSTALL_PODS=$(kubectl get pods -n "${CC_NS}" --no-headers 2>/dev/null \
        | grep -v controller-manager | awk '{print $1"="$3}' | tr '\n' ' ' || true)
    echo "  [${ELAPSED}s] kata installed: ${INSTALLED}/${SNP_TOTAL} node(s)  install-pods: ${INSTALL_PODS:-<none yet>}"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

if [[ "${INSTALLED}" -lt "${SNP_TOTAL}" ]]; then
    echo ""
    echo -e "${YELLOW}--- kata install diagnostics ---${NC}"
    echo "CcRuntime status:"
    kubectl get ccruntime ccruntime -o jsonpath='{.status}' 2>/dev/null | jq . 2>/dev/null | sed 's/^/  /' || true
    echo "Pods in ${CC_NS}:"
    kubectl get pods -n "${CC_NS}" -o wide 2>&1 | sed 's/^/  /' || true
    INSTALL_POD=$(kubectl get pods -n "${CC_NS}" --no-headers 2>/dev/null \
        | grep -v controller-manager | awk 'NR==1{print $1}' || true)
    if [[ -n "${INSTALL_POD}" ]]; then
        echo "Events for ${INSTALL_POD}:"
        kubectl describe pod "${INSTALL_POD}" -n "${CC_NS}" 2>/dev/null | sed -n '/Events:/,$p' | sed 's/^/  /' || true
        echo "Last 50 log lines for ${INSTALL_POD}:"
        kubectl logs "${INSTALL_POD}" -n "${CC_NS}" --tail=50 2>&1 | sed 's/^/  /' || true
    else
        echo -e "  ${YELLOW}⚠${NC} No install daemonset pod found — check ccNodeSelector/ccTolerations match the SNP node's labels/taints."
    fi
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
    step_fail "kata runtime installed on only ${INSTALLED}/${SNP_TOTAL} SNP node(s) after ${TIMEOUT}s"
fi
echo -e "${GREEN}✓${NC} kata runtime installed on all ${SNP_TOTAL} SNP node(s)"

step_ok "04 (./04-trustee-kbs.sh)"
