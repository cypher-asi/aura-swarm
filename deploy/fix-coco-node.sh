#!/bin/bash
# fix-coco-node.sh - Recover confidential (SNP) nodes wedged by a CoCo install.
#
# Symptom this fixes:
#   The kata pre-install (reqs-payload) restarted/replaced containerd on the SNP
#   node, which knocked out the VPC CNI IPAM daemon (aws-node). New pods then
#   fail to get networking:
#       aws-cni ... dial tcp 127.0.0.1:50051: connect: connection refused
#   and the CcRuntime kata install can never complete.
#
# What it does, for each unhealthy SNP node (label swarm.io/confidential-node=true):
#   1. Clears orphaned CoCo install/pre-install daemonsets+pods (when no CcRuntime).
#   2. Tries to recover networking by restarting that node's aws-node pod.
#   3. If the node still won't go healthy, terminates the EC2 instance so the
#      managed node group provisions a CLEAN replacement, and waits for it Ready.
#
# It is idempotent and safe to re-run. After it finishes, re-run
# ./03-coco-operator.sh (the operator reschedules the kata install on the
# recovered/replacement node).
#
# Usage:
#   ./fix-coco-node.sh                  # auto-detect & recover unhealthy SNP nodes
#   ./fix-coco-node.sh <node-name>      # operate on a specific node
#   FORCE_REPLACE=true ./fix-coco-node.sh [node]   # skip restart, replace outright
#   AWS_NODE_WAIT=120 NODE_JOIN_WAIT=1200 ./fix-coco-node.sh   # tune timeouts

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

CC_NS="confidential-containers-system"
SNP_LABEL="swarm.io/confidential-node=true"
AWS_NODE_WAIT="${AWS_NODE_WAIT:-120}"      # seconds to wait for aws-node to go Ready
NODE_JOIN_WAIT="${NODE_JOIN_WAIT:-1200}"   # seconds to wait for a replacement node
FORCE_REPLACE="${FORCE_REPLACE:-false}"
TARGET_NODE="${1:-}"

echo "=============================================="
echo "  Aura Swarm - CoCo SNP node recovery"
echo "=============================================="
echo ""

require_cmds aws kubectl jq
require_aws_auth
ensure_kubectl_context
echo ""

#------------------------------------------------------------------------------
# Helpers
#------------------------------------------------------------------------------

# 0 if an aws-node pod on $1 has all containers ready.
aws_node_ready_on() {
    local node="$1" cnt
    cnt=$(kubectl get pods -n kube-system -l k8s-app=aws-node -o json 2>/dev/null \
        | jq --arg n "${node}" '
            [ .items[]
              | select(.spec.nodeName == $n)
              | select((.status.containerStatuses // []) | length > 0)
              | select((.status.containerStatuses) | all(.ready == true))
            ] | length' 2>/dev/null || echo 0)
    [[ "${cnt:-0}" -ge 1 ]]
}

# 0 if the node's Ready condition is True.
node_ready() {
    local node="$1" s
    s=$(kubectl get node "${node}" -o json 2>/dev/null \
        | jq -r '[.status.conditions[]? | select(.type=="Ready")][0].status // "Unknown"' 2>/dev/null)
    [[ "${s}" == "True" ]]
}

node_instance_id() {
    kubectl get node "$1" -o jsonpath='{.spec.providerID}' 2>/dev/null | sed 's#.*/##'
}

snp_node_names() {
    kubectl get nodes -l "${SNP_LABEL}" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null
}

# Print phase, init/container states (with waiting reasons), recent events and
# the last log lines for a pod — so waits are never blind.
dump_pod_diag() {
    local ns="$1" pod="$2" loglines="${3:-12}"
    [[ -z "${pod}" ]] && return 0
    local phase
    phase=$(kubectl get pod -n "${ns}" "${pod}" -o jsonpath='{.status.phase}' 2>/dev/null)
    echo "      phase: ${phase:-<gone>}"
    kubectl get pod -n "${ns}" "${pod}" -o jsonpath='{range .status.initContainerStatuses[*]}      init[{.name}] ready={.ready} waiting={.state.waiting.reason} running={.state.running.startedAt} {.state.waiting.message}{"\n"}{end}' 2>/dev/null
    kubectl get pod -n "${ns}" "${pod}" -o jsonpath='{range .status.containerStatuses[*]}      ctr[{.name}] ready={.ready} waiting={.state.waiting.reason} {.state.waiting.message}{"\n"}{end}' 2>/dev/null
    local ev
    ev=$(kubectl get events -n "${ns}" --field-selector "involvedObject.name=${pod}" \
        -o jsonpath='{range .items[*]}      event: {.reason}: {.message}{"\n"}{end}' 2>/dev/null | tail -4)
    [[ -n "${ev}" ]] && { echo "      recent events:"; echo "${ev}"; }
    # Logs from every init + main container (current, then previous if crashed).
    local c
    for c in $(kubectl get pod -n "${ns}" "${pod}" -o jsonpath='{range .spec.initContainers[*]}{.name}{"\n"}{end}{range .spec.containers[*]}{.name}{"\n"}{end}' 2>/dev/null); do
        local out
        out=$(kubectl logs -n "${ns}" "${pod}" -c "${c}" --tail="${loglines}" 2>/dev/null)
        [[ -z "${out}" ]] && out=$(kubectl logs -n "${ns}" "${pod}" -c "${c}" --tail="${loglines}" --previous 2>/dev/null)
        if [[ -n "${out}" ]]; then
            echo "      logs[${c}]:"
            echo "${out}" | sed 's/^/        /'
        fi
    done
}

restart_aws_node_on() {
    local node="$1" pod
    pod=$(kubectl get pods -n kube-system -l k8s-app=aws-node -o json 2>/dev/null \
        | jq -r --arg n "${node}" '.items[] | select(.spec.nodeName==$n) | .metadata.name' | head -1)
    if [[ -n "${pod}" ]]; then
        echo "  Restarting aws-node pod ${pod}..."
        kubectl delete pod -n kube-system "${pod}" --ignore-not-found >/dev/null 2>&1 || true
    else
        echo "  No aws-node pod found on ${node} (daemonset will recreate it)."
    fi
    local elapsed=0 stuck_init=0
    while (( elapsed < AWS_NODE_WAIT )); do
        if aws_node_ready_on "${node}"; then
            echo -e "  ${GREEN}✓${NC} aws-node is Ready on ${node}"
            return 0
        fi
        sleep 10; elapsed=$((elapsed + 10))
        pod=$(kubectl get pods -n kube-system -l k8s-app=aws-node -o json 2>/dev/null \
            | jq -r --arg n "${node}" '.items[] | select(.spec.nodeName==$n) | .metadata.name' | head -1)
        echo "  [${elapsed}s] aws-node not Ready on ${node}; current state:"
        dump_pod_diag kube-system "${pod}"

        # If the init container can't even start, containerd on the node is
        # wedged — in-place restart cannot fix it. Bail early to replacement.
        local init_running
        init_running=$(kubectl get pod -n kube-system "${pod}" -o jsonpath='{.status.initContainerStatuses[0].state.running.startedAt}{.status.initContainerStatuses[0].state.terminated.reason}' 2>/dev/null)
        if [[ -z "${init_running}" ]]; then
            stuck_init=$((stuck_init + 1))
        else
            stuck_init=0
        fi
        if (( stuck_init >= 3 )); then
            echo -e "  ${YELLOW}⚠${NC} aws-node init container has not started after ${stuck_init} checks — containerd on ${node} is wedged; in-place recovery won't work."
            return 1
        fi
    done
    return 1
}

# Force a fresh sandbox attempt for any stuck CoCo install pods on the node.
bounce_coco_pods_on() {
    local node="$1" pods p
    pods=$(kubectl get pods -n "${CC_NS}" -o json 2>/dev/null \
        | jq -r --arg n "${node}" '.items[] | select(.spec.nodeName==$n) | select(.metadata.name | test("controller-manager") | not) | .metadata.name' 2>/dev/null)
    while IFS= read -r p; do
        [[ -z "${p}" ]] && continue
        echo "  Deleting stuck CoCo pod ${p} (forces a fresh sandbox attempt)..."
        kubectl delete pod -n "${CC_NS}" "${p}" --force --grace-period=0 >/dev/null 2>&1 || true
    done <<< "${pods}"
}

# Clear a stale ASG launch failure by bouncing desired 0 -> configured count.
# EKS keeps the DEGRADED health issue from the last failed attempt and won't
# retry on its own once capacity frees; the bounce forces a fresh launch.
bounce_nodegroup_desired() {
    local ng="${RESOURCE_PREFIX}-confidential-node-group"
    local min="${CONFIDENTIAL_NODE_MIN_COUNT:-0}"
    local max="${CONFIDENTIAL_NODE_MAX_COUNT:-3}"
    local want="${CONFIDENTIAL_NODE_DESIRED_COUNT:-1}"
    echo "    Bouncing node group desired count (0 -> ${want}) to clear the stale failed launch..."
    aws eks update-nodegroup-config --cluster-name "${EKS_CLUSTER_NAME}" --nodegroup-name "${ng}" \
        --region "${AWS_REGION}" --scaling-config "minSize=${min},maxSize=${max},desiredSize=0" >/dev/null 2>&1 || true
    local e=0 s
    while (( e < 300 )); do
        s=$(aws eks describe-nodegroup --cluster-name "${EKS_CLUSTER_NAME}" --nodegroup-name "${ng}" \
            --region "${AWS_REGION}" --query 'nodegroup.status' --output text 2>/dev/null || echo "?")
        [[ "${s}" == "ACTIVE" ]] && break
        sleep 15; e=$((e + 15))
        echo "      [bounce ${e}s] node group status: ${s}"
    done
    aws eks update-nodegroup-config --cluster-name "${EKS_CLUSTER_NAME}" --nodegroup-name "${ng}" \
        --region "${AWS_REGION}" --scaling-config "minSize=${min},maxSize=${max},desiredSize=${want}" >/dev/null 2>&1 || true
    echo "    Re-requested desiredSize=${want}; waiting for the launch..."
}

replace_node() {
    local node="$1" iid
    iid=$(node_instance_id "${node}")
    if [[ -z "${iid}" || "${iid}" != i-* ]]; then
        step_fail "could not resolve EC2 instance id for ${node} (got '${iid}')"
    fi
    echo -e "  ${YELLOW}↺${NC} Replacing node ${node} (instance ${iid})..."
    kubectl cordon "${node}" >/dev/null 2>&1 || true
    aws ec2 terminate-instances --region "${AWS_REGION}" --instance-ids "${iid}" >/dev/null
    echo "  Terminated ${iid}; waiting for the managed node group to provision a clean SNP node..."

    local elapsed=0 ready launch_fail_seen=0 bounced=0
    while (( elapsed < NODE_JOIN_WAIT )); do
        # Detect ASG launch failures (e.g. vCPU quota). While the old metal is
        # still terminating this is a transient overlap; once it's gone the
        # failure is a STALE ASG activity that we clear with a desired bounce.
        local health
        health=$(aws eks describe-nodegroup --cluster-name "${EKS_CLUSTER_NAME}" \
            --nodegroup-name "${RESOURCE_PREFIX}-confidential-node-group" --region "${AWS_REGION}" \
            --query 'nodegroup.health.issues' --output json 2>/dev/null || echo '[]')
        if echo "${health}" | jq -e '.[]? | select(.code=="AsgInstanceLaunchFailures")' >/dev/null 2>&1; then
            launch_fail_seen=$((launch_fail_seen + 1))
            local msg old_state
            msg=$(echo "${health}" | jq -r '[.[]? | select(.code=="AsgInstanceLaunchFailures") | .message][0]')
            old_state=$(aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "${iid}" \
                --query 'Reservations[].Instances[].State.Name' --output text 2>/dev/null || echo "unknown")
            echo -e "  ${YELLOW}⚠${NC} Node group launch failure (AsgInstanceLaunchFailures):"
            echo "      ${msg}" | sed 's/^/    /'
            echo "    Old instance ${iid} state: ${old_state}"

            if [[ "${old_state}" == "terminated" || "${old_state}" == "unknown" ]]; then
                # Capacity has been released; the failure is stale.
                if (( bounced == 0 )); then
                    echo "    Old capacity is released — auto-clearing the stale failed launch."
                    bounce_nodegroup_desired
                    bounced=1
                    launch_fail_seen=0
                elif (( launch_fail_seen >= 2 )); then
                    echo ""
                    echo "  Still failing AFTER a desired bounce — this is a real capacity/quota wall:"
                    echo "    • Free standard On-Demand vCPUs in this account/region (terminate unused instances)."
                    echo "    • Request an increase for EC2 quota L-1216C47A (Running On-Demand Standard instances);"
                    echo "      a single ${CONFIDENTIAL_NODE_INSTANCE_TYPE} needs 192 vCPU of headroom."
                    echo "    • Inspect usage:"
                    echo "        aws ec2 describe-instances --region ${AWS_REGION} --filters Name=instance-state-name,Values=running,pending --query 'Reservations[].Instances[].InstanceType' --output text | sort | uniq -c"
                    step_fail "replacement node cannot launch due to EC2 vCPU quota/capacity (see message above)"
                fi
            else
                echo "    Old instance still releasing capacity; will keep checking..."
            fi
        fi

        ready=$(kubectl get nodes -l "${SNP_LABEL}" -o json 2>/dev/null \
            | jq --arg old "${node}" '
                [ .items[]
                  | select(.metadata.name != $old)
                  | select(.status.conditions[]? | select(.type=="Ready" and .status=="True"))
                ] | length' 2>/dev/null || echo 0)
        if [[ "${ready:-0}" -ge 1 ]]; then
            local newname
            newname=$(kubectl get nodes -l "${SNP_LABEL}" -o json 2>/dev/null \
                | jq -r --arg old "${node}" '[.items[] | select(.metadata.name != $old) | select(.status.conditions[]? | select(.type=="Ready" and .status=="True")) | .metadata.name][0]')
            echo -e "  ${GREEN}✓${NC} Replacement SNP node is Ready: ${newname}"
            return 0
        fi
        sleep 30; elapsed=$((elapsed + 30))
        echo "  [${elapsed}s] waiting for a replacement SNP node to join and become Ready..."
        # EC2 lifecycle for this node group (the real signal: shutting-down -> pending -> running).
        echo "    EC2 instances (tag eks:nodegroup-name=${RESOURCE_PREFIX}-confidential-node-group):"
        aws ec2 describe-instances --region "${AWS_REGION}" \
            --filters "Name=tag:eks:nodegroup-name,Values=${RESOURCE_PREFIX}-confidential-node-group" \
                      "Name=instance-state-name,Values=pending,running,shutting-down,stopping,stopped" \
            --query 'Reservations[].Instances[].{id:InstanceId,state:State.Name,ip:PrivateIpAddress,launched:LaunchTime}' \
            --output text 2>/dev/null | sed 's/^/      /' || echo "      (unable to query EC2)"
        # k8s node join progress + kubelet readiness.
        echo "    SNP nodes in k8s:"
        kubectl get nodes -l "${SNP_LABEL}" \
            -o custom-columns='NAME:.metadata.name,READY:.status.conditions[-1].type,AGE:.metadata.creationTimestamp' \
            --no-headers 2>/dev/null | sed 's/^/      /' || echo "      (none registered yet)"
    done
    step_fail "no replacement SNP node became Ready within ${NODE_JOIN_WAIT}s (check the node group / EC2 console)"
}

#------------------------------------------------------------------------------
# Clean up orphaned CoCo daemonsets (operator sometimes leaves them when a
# CcRuntime delete was interrupted) so broken pods stop thrashing.
#------------------------------------------------------------------------------
if ! kubectl get ccruntime ccruntime >/dev/null 2>&1; then
    if kubectl get ds -n "${CC_NS}" -o name 2>/dev/null | grep -q daemon; then
        echo "No CcRuntime present but install daemonsets remain — clearing orphans..."
        kubectl delete ds -n "${CC_NS}" --all --ignore-not-found >/dev/null 2>&1 || true
        kubectl delete pods -n "${CC_NS}" -l 'app.kubernetes.io/managed-by=cc-operator' --force --grace-period=0 --ignore-not-found >/dev/null 2>&1 || true
        echo -e "${GREEN}✓${NC} Orphaned CoCo daemonsets cleared"
        echo ""
    fi
fi

#------------------------------------------------------------------------------
# Determine which SNP node(s) to act on
#------------------------------------------------------------------------------
NODES=()
if [[ -n "${TARGET_NODE}" ]]; then
    NODES=("${TARGET_NODE}")
else
    while IFS= read -r n; do
        [[ -z "${n}" ]] && continue
        NODES+=("${n}")
    done < <(snp_node_names)
fi

if [[ ${#NODES[@]} -eq 0 ]]; then
    echo -e "${YELLOW}⚠${NC} No confidential (SNP) nodes found (label ${SNP_LABEL})."
    echo "  Run ./02-snp-node-group.sh first, or pass a node name explicitly."
    exit 0
fi

echo "SNP node(s) to inspect: ${NODES[*]}"
echo ""

#------------------------------------------------------------------------------
# Recover each node
#------------------------------------------------------------------------------
RECOVERED=0
REPLACED=0
for node in "${NODES[@]}"; do
    echo -e "${CYAN}Node ${node}${NC}"

    healthy=false
    if [[ "${FORCE_REPLACE}" != "true" ]] && node_ready "${node}" && aws_node_ready_on "${node}"; then
        echo -e "  ${GREEN}✓${NC} Node Ready and aws-node healthy — networking looks fine."
        healthy=true
    fi

    if [[ "${healthy}" != "true" ]]; then
        if [[ "${FORCE_REPLACE}" == "true" ]]; then
            echo "  FORCE_REPLACE=true — skipping restart, replacing node."
            replace_node "${node}"
            REPLACED=$((REPLACED + 1))
            continue
        fi

        echo "  aws-node/CNI unhealthy on ${node} — attempting in-place recovery..."
        if restart_aws_node_on "${node}"; then
            bounce_coco_pods_on "${node}"
            RECOVERED=$((RECOVERED + 1))
        else
            echo -e "  ${YELLOW}⚠${NC} aws-node did not recover within ${AWS_NODE_WAIT}s — replacing the node."
            replace_node "${node}"
            REPLACED=$((REPLACED + 1))
        fi
    fi
    echo ""
done

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------
echo "=============================================="
echo -e "${GREEN}CoCo node recovery complete${NC}"
echo "  Restarted networking on: ${RECOVERED} node(s)"
echo "  Replaced:                ${REPLACED} node(s)"
echo "=============================================="
echo ""
echo "Next: re-run the CoCo install (it reschedules kata-deploy on the healthy node):"
echo "    ./03-coco-operator.sh"
