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
    echo "  Restarting Agent Pods (graceful)"
    echo "=============================================="
    echo ""
    
    GATEWAY_URL=$(kubectl get svc aura-swarm-gateway -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || echo "")
    
    if [[ -z "$GATEWAY_URL" ]]; then
        echo -e "${YELLOW}⚠${NC} Could not determine gateway URL."
        echo "  Agents will pick up the new image on next manual restart."
    else
        GATEWAY_URL="http://${GATEWAY_URL}"
        echo "Gateway URL: ${GATEWAY_URL}"
        echo ""
        
        AGENT_IDS=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent \
            --no-headers -o jsonpath='{range .items[*]}{.metadata.annotations.swarm\.io/agent-id-full}{"\n"}{end}' 2>/dev/null || echo "")
        
        if [[ -z "$AGENT_IDS" ]]; then
            echo "No running agent pods found in ${K8S_NAMESPACE_AGENTS}."
        else
            AGENT_COUNT=$(echo "$AGENT_IDS" | grep -c . || echo "0")
            echo "Found ${AGENT_COUNT} running agent(s). Restarting via gateway API..."
            echo ""
            
            for AGENT_ID in $AGENT_IDS; do
                [[ -z "$AGENT_ID" ]] && continue
                echo -n "  Restarting agent ${AGENT_ID:0:16}... "
                HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
                    -X POST "${GATEWAY_URL}/v1/agents/${AGENT_ID}/restart" \
                    -H "Content-Type: application/json" 2>/dev/null || echo "000")
                
                if [[ "$HTTP_CODE" == "200" ]]; then
                    echo -e "${GREEN}✓${NC}"
                else
                    echo -e "${YELLOW}⚠ HTTP ${HTTP_CODE}${NC}"
                fi
            done
            
            echo ""
            echo -e "${GREEN}✓${NC} Agent restart requests sent. Pods will be recreated with the new image."
        fi
    fi
fi

echo ""
echo -e "${GREEN}Refresh complete!${NC}"
