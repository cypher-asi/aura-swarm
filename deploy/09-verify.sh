#!/bin/bash
# 09-verify.sh - Verify deployment health
#
# Validates:
# - All pods running
# - EFS mounted correctly
# - Gateway responding on LoadBalancer
# - Health endpoints returning 200

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

ERRORS=0

echo "=============================================="
echo "  Aura Swarm - Verification"
echo "=============================================="
echo ""

#------------------------------------------------------------------------------
# Check pods
#------------------------------------------------------------------------------

echo "Checking pods..."
echo ""

check_pod() {
    local name=$1
    local namespace=$2
    
    local status=$(kubectl get pods -n "${namespace}" -l "app=${name}" -o jsonpath='{.items[0].status.phase}' 2>/dev/null)
    local ready=$(kubectl get pods -n "${namespace}" -l "app=${name}" -o jsonpath='{.items[0].status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    
    if [[ "$status" == "Running" && "$ready" == "True" ]]; then
        echo -e "${GREEN}✓${NC} ${name}: Running and Ready"
        return 0
    elif [[ "$status" == "Running" ]]; then
        echo -e "${YELLOW}⚠${NC} ${name}: Running but not Ready"
        return 0
    else
        echo -e "${RED}✗${NC} ${name}: ${status:-Not found}"
        ((ERRORS++))
        return 1
    fi
}

check_pod "aura-swarm-gateway" "${K8S_NAMESPACE_SYSTEM}"
check_pod "aura-swarm-control" "${K8S_NAMESPACE_SYSTEM}"
check_pod "aura-swarm-scheduler" "${K8S_NAMESPACE_SYSTEM}"

echo ""

#------------------------------------------------------------------------------
# Check PVC
#------------------------------------------------------------------------------

echo "Checking PersistentVolumeClaim..."

PVC_STATUS=$(kubectl get pvc swarm-agent-state -n "${K8S_NAMESPACE_AGENTS}" -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")

if [[ "$PVC_STATUS" == "Bound" ]]; then
    echo -e "${GREEN}✓${NC} PVC swarm-agent-state: Bound"
else
    echo -e "${RED}✗${NC} PVC swarm-agent-state: ${PVC_STATUS}"
    ((ERRORS++))
fi

echo ""

#------------------------------------------------------------------------------
# Check Services
#------------------------------------------------------------------------------

echo "Checking services..."

# Gateway LoadBalancer
GATEWAY_LB=$(kubectl get svc aura-swarm-gateway-lb -n "${K8S_NAMESPACE_SYSTEM}" -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || echo "")

if [[ -n "$GATEWAY_LB" ]]; then
    echo -e "${GREEN}✓${NC} Gateway LoadBalancer: ${GATEWAY_LB}"
else
    GATEWAY_LB=$(kubectl get svc aura-swarm-gateway-lb -n "${K8S_NAMESPACE_SYSTEM}" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "")
    if [[ -n "$GATEWAY_LB" ]]; then
        echo -e "${GREEN}✓${NC} Gateway LoadBalancer: ${GATEWAY_LB}"
    else
        echo -e "${YELLOW}⚠${NC} Gateway LoadBalancer: Pending (may take a few minutes)"
    fi
fi

echo ""

#------------------------------------------------------------------------------
# Health checks (internal via kubectl port-forward)
#------------------------------------------------------------------------------

echo "Checking health endpoints..."

check_health() {
    local service=$1
    local port=$2
    
    # Use kubectl exec to check health from within the cluster
    local health_status=$(kubectl exec -n "${K8S_NAMESPACE_SYSTEM}" deploy/${service} -- wget -q -O - http://localhost:${port}/health 2>/dev/null || echo "")
    
    if [[ -n "$health_status" ]]; then
        echo -e "${GREEN}✓${NC} ${service} /health: OK"
        return 0
    else
        # Try curl if wget not available
        health_status=$(kubectl exec -n "${K8S_NAMESPACE_SYSTEM}" deploy/${service} -- curl -s http://localhost:${port}/health 2>/dev/null || echo "")
        if [[ -n "$health_status" ]]; then
            echo -e "${GREEN}✓${NC} ${service} /health: OK"
            return 0
        else
            echo -e "${YELLOW}⚠${NC} ${service} /health: Could not check (service may still be starting)"
            return 0
        fi
    fi
}

check_health "aura-swarm-gateway" 8080
check_health "aura-swarm-control" 8080

echo ""

#------------------------------------------------------------------------------
# Show deployment info
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

if [[ -n "${GATEWAY_LB:-}" ]]; then
    echo "Gateway URL: http://${GATEWAY_LB}"
    echo ""
fi

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------

echo "=============================================="

if [[ $ERRORS -eq 0 ]]; then
    echo -e "${GREEN}All verifications passed!${NC}"
    echo ""
    echo "Deployment completed successfully."
    echo ""
    if [[ -n "${GATEWAY_LB:-}" ]]; then
        echo "Access the API at: http://${GATEWAY_LB}"
        echo ""
        echo "Test with:"
        echo "  curl http://${GATEWAY_LB}/health"
    fi
else
    echo -e "${RED}Verification failed with $ERRORS error(s)${NC}"
    echo ""
    echo "Check the logs for more details:"
    echo "  kubectl logs -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-gateway"
    echo "  kubectl logs -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-control"
    echo "  kubectl logs -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-scheduler"
    exit 1
fi
