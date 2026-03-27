#!/bin/bash
# 09-verify.sh - Verify deployment health
#
# Validates:
# - EFS CSI driver running
# - All PVCs bound (gateway-data, control-data, agent-state)
# - All pods running
# - Gateway responding on LoadBalancer
# - Health endpoints returning 200
#
# On failure, prints targeted diagnostics (pod events, PVC events, CSI logs)
# so the operator can identify the root cause without manual kubectl triage.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

ERRORS=0
WARNINGS=0

echo "=============================================="
echo "  Aura Swarm - Verification"
echo "=============================================="
echo ""

#------------------------------------------------------------------------------
# Check swarm-agent harness digest convergence
#------------------------------------------------------------------------------

echo -e "${CYAN}Swarm Agent Harness Digest${NC}"

EXPECTED_HARNESS_IMAGE=$(kubectl get configmap aura-swarm-config -n "${K8S_NAMESPACE_SYSTEM}" \
    -o jsonpath='{.data.AURA_HARNESS_IMAGE}' 2>/dev/null || echo "")

EXPECTED_DIGEST=""
if [[ "${EXPECTED_HARNESS_IMAGE}" =~ sha256:[a-f0-9]{64} ]]; then
    EXPECTED_DIGEST="${BASH_REMATCH[0]}"
fi

AGENT_IMAGE_ROWS=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent \
    -o jsonpath='{range .items[*]}{.metadata.name}{"|"}{.status.containerStatuses[0].imageID}{"\n"}{end}' 2>/dev/null || echo "")

if [[ -z "${AGENT_IMAGE_ROWS}" ]]; then
    echo -e "${YELLOW}⚠${NC} No swarm-agent pods found; digest convergence check skipped"
    ((WARNINGS++))
else
    declare -A DIGEST_COUNTS=()
    while IFS='|' read -r pod_name image_id; do
        [[ -z "${pod_name}" ]] && continue
        if [[ "${image_id}" =~ sha256:[a-f0-9]{64} ]]; then
            digest="${BASH_REMATCH[0]}"
            DIGEST_COUNTS["$digest"]=$(( ${DIGEST_COUNTS["$digest"]:-0} + 1 ))
        else
            echo -e "${YELLOW}⚠${NC} ${pod_name}: could not parse digest from imageID (${image_id:-missing})"
            ((WARNINGS++))
        fi
    done <<< "${AGENT_IMAGE_ROWS}"

    DIGEST_TOTAL=${#DIGEST_COUNTS[@]}
    if [[ "${DIGEST_TOTAL}" -eq 1 ]]; then
        for digest in "${!DIGEST_COUNTS[@]}"; do
            echo -e "${GREEN}✓${NC} All swarm-agent pods use digest: ${digest}"
            if [[ -n "${EXPECTED_DIGEST}" && "${EXPECTED_DIGEST}" != "${digest}" ]]; then
                echo -e "${RED}✗${NC} ConfigMap digest (${EXPECTED_DIGEST}) does not match running pods"
                echo "    Remediation: run ./08-deploy-k8s.sh --recreate-agents"
                ((ERRORS++))
            fi
        done
    elif [[ "${DIGEST_TOTAL}" -gt 1 ]]; then
        echo -e "${RED}✗${NC} Mixed swarm-agent digests detected (${DIGEST_TOTAL} unique)"
        for digest in "${!DIGEST_COUNTS[@]}"; do
            echo "    ${digest} (${DIGEST_COUNTS[$digest]} pod(s))"
        done
        echo "    Remediation: run ./08-deploy-k8s.sh --recreate-agents"
        ((ERRORS++))
    else
        echo -e "${YELLOW}⚠${NC} Could not parse any swarm-agent image digests"
        ((WARNINGS++))
    fi
fi

echo ""

#------------------------------------------------------------------------------
# Check EFS CSI driver
#------------------------------------------------------------------------------

echo -e "${CYAN}EFS CSI Driver${NC}"

EFS_CSI_TOTAL=$(kubectl get pods -n kube-system -l app=efs-csi-controller --no-headers 2>/dev/null | wc -l | tr -d ' ')
EFS_CSI_RUNNING=$(kubectl get pods -n kube-system -l app=efs-csi-controller --no-headers 2>/dev/null | grep -c "Running" || true)

if [[ "$EFS_CSI_TOTAL" -eq 0 ]]; then
    echo -e "${RED}✗${NC} EFS CSI controller: not installed"
    echo "    Fix: run ./05-configure-eks.sh"
    ((ERRORS++))
elif [[ "$EFS_CSI_RUNNING" -eq "$EFS_CSI_TOTAL" ]]; then
    echo -e "${GREEN}✓${NC} EFS CSI controller: ${EFS_CSI_RUNNING}/${EFS_CSI_TOTAL} Running"
else
    echo -e "${YELLOW}⚠${NC} EFS CSI controller: ${EFS_CSI_RUNNING}/${EFS_CSI_TOTAL} Running"
    kubectl get pods -n kube-system -l app=efs-csi-controller --no-headers 2>/dev/null | while read -r line; do
        echo "    $line"
    done
    ((WARNINGS++))
fi

echo ""

#------------------------------------------------------------------------------
# Check PVCs
#------------------------------------------------------------------------------

echo -e "${CYAN}PersistentVolumeClaims${NC}"

check_pvc() {
    local pvc_name=$1
    local namespace=$2

    local status
    status=$(kubectl get pvc "$pvc_name" -n "$namespace" -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")

    if [[ "$status" == "Bound" ]]; then
        echo -e "${GREEN}✓${NC} ${pvc_name}: Bound"
        return 0
    else
        echo -e "${RED}✗${NC} ${pvc_name}: ${status}"
        ((ERRORS++))

        # Show the most recent provisioning event to explain why
        LAST_EVENT=$(kubectl get events -n "$namespace" \
            --field-selector "involvedObject.name=${pvc_name},reason=ProvisioningFailed" \
            --sort-by='.lastTimestamp' -o jsonpath='{.items[-1:].message}' 2>/dev/null || echo "")

        if [[ -n "$LAST_EVENT" ]]; then
            echo "    Last event: ${LAST_EVENT:0:200}"

            # Detect common failure patterns and suggest fixes
            if echo "$LAST_EVENT" | grep -q "AssumeRoleWithWebIdentity"; then
                echo ""
                echo -e "    ${YELLOW}Diagnosis: IRSA trust policy mismatch${NC}"
                echo "    The EFS CSI driver's IAM role trust policy doesn't match the"
                echo "    cluster's OIDC provider. This happens when the cluster was"
                echo "    rebuilt but the IAM role wasn't updated."
                echo "    Fix: re-run ./05-configure-eks.sh (reconciles the trust policy)"
            elif echo "$LAST_EVENT" | grep -q "AccessDenied"; then
                echo ""
                echo -e "    ${YELLOW}Diagnosis: IAM permission denied${NC}"
                echo "    The EFS CSI driver role lacks permission to create access points."
                echo "    Fix: ensure AmazonEFSCSIDriverPolicy is attached to the role"
            fi
        fi
        return 1
    fi
}

check_pvc "aura-swarm-gateway-data" "${K8S_NAMESPACE_SYSTEM}" || true
check_pvc "aura-swarm-control-data" "${K8S_NAMESPACE_SYSTEM}" || true
check_pvc "swarm-agent-state" "${K8S_NAMESPACE_AGENTS}" || true

echo ""

#------------------------------------------------------------------------------
# Check pods
#------------------------------------------------------------------------------

echo -e "${CYAN}Pods${NC}"

check_pod() {
    local name=$1
    local namespace=$2

    local phase
    phase=$(kubectl get pods -n "$namespace" -l "app=${name}" -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "")
    local ready
    ready=$(kubectl get pods -n "$namespace" -l "app=${name}" -o jsonpath='{.items[0].status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "")

    if [[ "$phase" == "Running" && "$ready" == "True" ]]; then
        echo -e "${GREEN}✓${NC} ${name}: Running and Ready"
        return 0
    elif [[ "$phase" == "Running" ]]; then
        echo -e "${YELLOW}⚠${NC} ${name}: Running but not Ready"
        ((WARNINGS++))
        return 0
    else
        echo -e "${RED}✗${NC} ${name}: ${phase:-Not found}"
        ((ERRORS++))

        # Show why the pod is stuck
        if [[ "$phase" == "Pending" ]]; then
            POD_NAME=$(kubectl get pods -n "$namespace" -l "app=${name}" \
                -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
            if [[ -n "$POD_NAME" ]]; then
                # Check for scheduling failures
                SCHEDULE_MSG=$(kubectl get events -n "$namespace" \
                    --field-selector "involvedObject.name=${POD_NAME}" \
                    --sort-by='.lastTimestamp' -o jsonpath='{.items[-1:].message}' 2>/dev/null || echo "")
                if [[ -n "$SCHEDULE_MSG" ]]; then
                    echo "    Last event: ${SCHEDULE_MSG:0:200}"
                fi

                # Check if it's waiting on a PVC
                WAITING_VOLS=$(kubectl get pod "$POD_NAME" -n "$namespace" \
                    -o jsonpath='{.status.conditions[?(@.reason=="Unschedulable")].message}' 2>/dev/null || echo "")
                if [[ -n "$WAITING_VOLS" ]]; then
                    echo "    ${WAITING_VOLS:0:200}"
                fi
            fi
        fi
        return 1
    fi
}

check_pod "aura-swarm-gateway" "${K8S_NAMESPACE_SYSTEM}" || true
check_pod "aura-swarm-control" "${K8S_NAMESPACE_SYSTEM}" || true
check_pod "aura-swarm-scheduler" "${K8S_NAMESPACE_SYSTEM}" || true

echo ""

#------------------------------------------------------------------------------
# Check Services
#------------------------------------------------------------------------------

echo -e "${CYAN}Services${NC}"

GATEWAY_LB=$(kubectl get svc aura-swarm-gateway-lb -n "${K8S_NAMESPACE_SYSTEM}" \
    -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || echo "")

if [[ -z "$GATEWAY_LB" ]]; then
    GATEWAY_LB=$(kubectl get svc aura-swarm-gateway-lb -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "")
fi

if [[ -n "$GATEWAY_LB" ]]; then
    echo -e "${GREEN}✓${NC} Gateway LoadBalancer: ${GATEWAY_LB}"
else
    echo -e "${YELLOW}⚠${NC} Gateway LoadBalancer: Pending (may take a few minutes)"
    ((WARNINGS++))
fi

echo ""

#------------------------------------------------------------------------------
# Health checks (internal via kubectl exec)
#------------------------------------------------------------------------------

echo -e "${CYAN}Health Endpoints${NC}"

check_health() {
    local service=$1
    local port=$2

    # Skip health check if the pod isn't running
    local phase
    phase=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -l "app=${service}" \
        -o jsonpath='{.items[0].status.phase}' 2>/dev/null || echo "")
    if [[ "$phase" != "Running" ]]; then
        echo -e "${YELLOW}⚠${NC} ${service} /health: skipped (pod is ${phase:-missing})"
        return 0
    fi

    local health_status
    health_status=$(kubectl exec -n "${K8S_NAMESPACE_SYSTEM}" "deploy/${service}" \
        -- wget -q -O - "http://localhost:${port}/health" 2>/dev/null || echo "")

    if [[ -n "$health_status" ]]; then
        echo -e "${GREEN}✓${NC} ${service} /health: OK"
        return 0
    fi

    health_status=$(kubectl exec -n "${K8S_NAMESPACE_SYSTEM}" "deploy/${service}" \
        -- curl -sf "http://localhost:${port}/health" 2>/dev/null || echo "")

    if [[ -n "$health_status" ]]; then
        echo -e "${GREEN}✓${NC} ${service} /health: OK"
    else
        echo -e "${YELLOW}⚠${NC} ${service} /health: no response (service may still be starting)"
        ((WARNINGS++))
    fi
}

check_health "aura-swarm-gateway" 8080
check_health "aura-swarm-control" 8080

echo ""

#------------------------------------------------------------------------------
# Deployment info
#------------------------------------------------------------------------------

echo "=============================================="
echo "  Deployment Information"
echo "=============================================="
echo ""

echo "Cluster: ${EKS_CLUSTER_NAME}"
echo "Region: ${AWS_REGION}"
echo ""

echo "Namespaces:"
kubectl get ns | grep -E "swarm|NAME"
echo ""

echo "Pods:"
kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -o wide
echo ""

echo "PVCs:"
kubectl get pvc -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null || true
kubectl get pvc -n "${K8S_NAMESPACE_AGENTS}" 2>/dev/null || true
echo ""

if [[ -n "${GATEWAY_LB:-}" ]]; then
    echo "Gateway URL: http://${GATEWAY_LB}"
    echo ""
fi

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------

echo "=============================================="

if [[ $ERRORS -eq 0 && $WARNINGS -eq 0 ]]; then
    echo -e "${GREEN}All verifications passed!${NC}"
    echo ""
    echo "Deployment completed successfully."
    if [[ -n "${GATEWAY_LB:-}" ]]; then
        echo ""
        echo "Access the API at: http://${GATEWAY_LB}"
        echo "  curl http://${GATEWAY_LB}/health"
    fi
elif [[ $ERRORS -eq 0 ]]; then
    echo -e "${YELLOW}Passed with ${WARNINGS} warning(s)${NC}"
    echo ""
    echo "Deployment is functional but some checks need attention (see above)."
else
    echo -e "${RED}Verification failed: ${ERRORS} error(s), ${WARNINGS} warning(s)${NC}"
    echo ""
    echo "Troubleshooting commands:"
    echo "  kubectl logs  -n kube-system -l app=efs-csi-controller --tail=20"
    echo "  kubectl describe pvc -n ${K8S_NAMESPACE_SYSTEM}"
    echo "  kubectl describe pod -l app=aura-swarm-gateway -n ${K8S_NAMESPACE_SYSTEM}"
    echo "  kubectl logs  -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-gateway"
    echo "  kubectl logs  -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-control"
    echo ""
    echo "Quick fix: ./fix-pending-pods.sh  (repairs IRSA trust policy + reprovisions PVCs)"
    exit 1
fi
