#!/bin/bash
# refresh-k8s.sh - Restart deployments to pick up new images
#
# Use this after pushing new images to ECR to force Kubernetes
# to pull the latest images.
#
# Usage:
#   ./refresh-k8s.sh              # Restart all deployments
#   ./refresh-k8s.sh gateway      # Restart only gateway
#   ./refresh-k8s.sh control      # Restart only control
#   ./refresh-k8s.sh scheduler    # Restart only scheduler

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Refresh Kubernetes Deployments"
echo "=============================================="
echo ""

# Parse arguments - which services to restart
SERVICES=()
if [[ $# -eq 0 ]]; then
    SERVICES=("gateway" "control" "scheduler")
else
    SERVICES=("$@")
fi

#------------------------------------------------------------------------------
# Restart deployments
#------------------------------------------------------------------------------

echo "Restarting deployments to pull latest images..."
echo ""

for service in "${SERVICES[@]}"; do
    DEPLOYMENT="aura-swarm-${service}"
    
    echo "Restarting ${DEPLOYMENT}..."
    if kubectl rollout restart deployment/${DEPLOYMENT} -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null; then
        echo -e "${GREEN}✓${NC} Triggered restart for ${DEPLOYMENT}"
    else
        echo -e "${RED}✗${NC} Failed to restart ${DEPLOYMENT}"
    fi
done

echo ""

#------------------------------------------------------------------------------
# Wait for rollouts to complete
#------------------------------------------------------------------------------

echo "Waiting for rollouts to complete..."
echo ""

for service in "${SERVICES[@]}"; do
    DEPLOYMENT="aura-swarm-${service}"
    
    echo "Waiting for ${DEPLOYMENT}..."
    if kubectl rollout status deployment/${DEPLOYMENT} -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s; then
        echo -e "${GREEN}✓${NC} ${DEPLOYMENT} is ready"
    else
        echo -e "${YELLOW}⚠${NC} ${DEPLOYMENT} rollout timed out or failed"
    fi
done

echo ""

#------------------------------------------------------------------------------
# Show current status
#------------------------------------------------------------------------------

echo "=============================================="
echo "  Current Pod Status"
echo "=============================================="
echo ""

kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -o wide

echo ""

# Show recent events if there are any issues
PROBLEM_PODS=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" --no-headers 2>/dev/null | grep -v "Running" | wc -l)
if [[ $PROBLEM_PODS -gt 0 ]]; then
    echo -e "${YELLOW}Some pods are not running. Recent events:${NC}"
    kubectl get events -n "${K8S_NAMESPACE_SYSTEM}" --sort-by='.lastTimestamp' | tail -10
fi

echo ""
echo -e "${GREEN}Refresh complete!${NC}"
