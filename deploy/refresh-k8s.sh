#!/bin/bash
# refresh-k8s.sh - Restart deployments to pick up new images
#
# Use this after pushing new images to ECR to force Kubernetes
# to pull the latest images.
#
# Usage:
#   ./refresh-k8s.sh              # Restart all platform deployments
#   ./refresh-k8s.sh gateway      # Restart only gateway
#   ./refresh-k8s.sh control      # Restart only control
#   ./refresh-k8s.sh scheduler    # Restart only scheduler
#   ./refresh-k8s.sh --agents     # Restart agent pods (after runtime update)
#   ./refresh-k8s.sh --all        # Restart platform + agents

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
REFRESH_AGENTS=false

for arg in "$@"; do
    case $arg in
        --agents)
            REFRESH_AGENTS=true
            ;;
        --all)
            SERVICES=("gateway" "control" "scheduler")
            REFRESH_AGENTS=true
            ;;
        *)
            SERVICES+=("$arg")
            ;;
    esac
done

if [[ ${#SERVICES[@]} -eq 0 && "$REFRESH_AGENTS" == "false" ]]; then
    SERVICES=("gateway" "control" "scheduler")
fi

#------------------------------------------------------------------------------
# Restart platform deployments (if any specified)
#------------------------------------------------------------------------------

if [[ ${#SERVICES[@]} -gt 0 ]]; then
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
fi

echo ""

# Show recent events if there are any issues
PROBLEM_PODS=$(kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" --no-headers 2>/dev/null | grep -v "Running" | wc -l)
if [[ $PROBLEM_PODS -gt 0 ]]; then
    echo -e "${YELLOW}Some pods are not running. Recent events:${NC}"
    kubectl get events -n "${K8S_NAMESPACE_SYSTEM}" --sort-by='.lastTimestamp' | tail -10
fi

#------------------------------------------------------------------------------
# Restart agent pods (if requested)
#------------------------------------------------------------------------------

if [[ "$REFRESH_AGENTS" == "true" ]]; then
    echo ""
    echo "=============================================="
    echo "  Restarting Agent Pods"
    echo "=============================================="
    echo ""
    
    AGENT_COUNT=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent --no-headers 2>/dev/null | wc -l || echo "0")
    
    if [[ "$AGENT_COUNT" -gt 0 ]]; then
        echo "Found ${AGENT_COUNT} running agent pod(s)."
        echo "Deleting all agent pods to pull new runtime image..."
        echo ""
        
        kubectl delete pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent --wait=false
        
        echo ""
        echo -e "${GREEN}✓${NC} Agent pods deleted."
        echo ""
        echo "Agent pods in ${K8S_NAMESPACE_AGENTS}:"
        kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent 2>/dev/null || echo "  (no pods)"
        echo ""
        echo -e "${YELLOW}Note:${NC} Agents will be recreated with new image when sessions are opened."
    else
        echo "No running agent pods found in ${K8S_NAMESPACE_AGENTS}."
    fi
fi

echo ""
echo -e "${GREEN}Refresh complete!${NC}"
