#!/bin/bash
# 07-build-images.sh - Build and push container images
#
# Builds:
# - aura-swarm-gateway
# - aura-swarm-control
# - aura-swarm-scheduler
# - aura-harness (optional, from ../aura-harness)
#
# Pushes to ECR repositories
#
# Usage:
#   ./07-build-images.sh              # Build platform services only
#   ./07-build-images.sh --all        # Build platform + aura-harness
#   ./07-build-images.sh --harness    # Build aura-harness only
#   ./07-build-images.sh --harness --refresh  # Build harness AND restart agent pods

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Parse arguments
BUILD_PLATFORM=true
DEV_MODE=false
REFRESH_K8S=false
REFRESH_GATEWAY_ONLY=false
AURA_HARNESS_PATH="${PROJECT_ROOT}/../aura-harness"
BUILD_HARNESS=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --all)
            BUILD_HARNESS=true
            shift
            ;;
        --harness)
            BUILD_PLATFORM=false
            BUILD_HARNESS=true
            shift
            ;;
        --harness-path)
            AURA_HARNESS_PATH="$2"
            shift 2
            ;;
        --dev-mode)
            DEV_MODE=true
            shift
            ;;
        --refresh)
            REFRESH_K8S=true
            shift
            ;;
        --refresh-gateway)
            REFRESH_K8S=true
            REFRESH_GATEWAY_ONLY=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--all] [--harness] [--harness-path PATH] [--dev-mode] [--refresh] [--refresh-gateway]"
            exit 1
            ;;
    esac
done

echo "=============================================="
echo "  Aura Swarm - Build and Push Images"
echo "=============================================="
echo ""
echo "Build platform services: ${BUILD_PLATFORM}"
echo "Build aura-harness: ${BUILD_HARNESS}"
echo "Dev mode (mock auth): ${DEV_MODE}"
echo "Refresh K8s after build: ${REFRESH_K8S}$(if [[ "$REFRESH_GATEWAY_ONLY" == "true" ]]; then echo " (gateway only)"; fi)"
echo ""

if [[ "$DEV_MODE" == "true" ]]; then
    echo -e "${YELLOW}⚠ DEV MODE ENABLED${NC}"
    echo "  - Gateway will use mock JWT validator"
    echo "  - Use tokens: test-token:<identity-uuid>:<namespace-uuid>"
    echo "  - No ZID server required"
    echo ""
fi

cd "${PROJECT_ROOT}"

#------------------------------------------------------------------------------
# Get ECR registry URL
#------------------------------------------------------------------------------

AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
ECR_REGISTRY="${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"

echo "ECR Registry: ${ECR_REGISTRY}"
echo "Image Tag: ${IMAGE_TAG}"
echo ""

#------------------------------------------------------------------------------
# Verify Docker is running
#------------------------------------------------------------------------------

if ! docker info &> /dev/null; then
    echo -e "${RED}✗ Docker is not running.${NC}"
    echo ""
    echo "  Start Docker Desktop and wait for it to be ready, then re-run this script."
    exit 1
fi

#------------------------------------------------------------------------------
# Authenticate with ECR
#------------------------------------------------------------------------------

echo "Authenticating with ECR..."
aws ecr get-login-password --region "${AWS_REGION}" | docker login --username AWS --password-stdin "${ECR_REGISTRY}"
echo -e "${GREEN}✓${NC} ECR authentication successful"
echo ""

#------------------------------------------------------------------------------
# Build platform services
#------------------------------------------------------------------------------

if [[ "$BUILD_PLATFORM" == "true" ]]; then
    echo "Building platform service images (multi-stage Docker build)..."
    echo ""

    # Services to build
    SERVICES=("gateway" "control" "scheduler")

    # Determine cargo features for gateway
    GATEWAY_FEATURES=""
    if [[ "$DEV_MODE" == "true" ]]; then
        GATEWAY_FEATURES="--features dev-mode"
    fi

    for service in "${SERVICES[@]}"; do
        IMAGE_NAME="${RESOURCE_PREFIX}-${service}"
        FULL_IMAGE="${ECR_REGISTRY}/${IMAGE_NAME}:${IMAGE_TAG}"
        
        # Set features for this service
        CARGO_FEATURES=""
        if [[ "$service" == "gateway" && "$DEV_MODE" == "true" ]]; then
            CARGO_FEATURES="--features dev-mode"
        fi
        
        echo ""
        echo "Building ${service} image..."
        if [[ -n "$CARGO_FEATURES" ]]; then
            echo "  Cargo features: ${CARGO_FEATURES}"
        fi
        
        # Multi-stage build: compile Rust in container, then create minimal runtime image
        docker build \
            --no-cache \
            --build-arg SERVICE="${service}" \
            --build-arg CARGO_FEATURES="${CARGO_FEATURES}" \
            -t "${IMAGE_NAME}:${IMAGE_TAG}" \
            -t "${FULL_IMAGE}" \
            -f "${PROJECT_ROOT}/docker/Dockerfile" "${PROJECT_ROOT}"
        
        echo -e "${GREEN}✓${NC} Built ${IMAGE_NAME}:${IMAGE_TAG}"
        
        echo "Pushing to ECR..."
        docker push "${FULL_IMAGE}"
        
        echo -e "${GREEN}✓${NC} Pushed ${FULL_IMAGE}"
    done
fi

#------------------------------------------------------------------------------
# Build aura-harness
#------------------------------------------------------------------------------

if [[ "$BUILD_HARNESS" == "true" ]]; then
    echo ""
    echo "=============================================="
    echo "  Building Aura Harness"
    echo "=============================================="
    echo ""
    
    if [[ ! -d "${AURA_HARNESS_PATH}" ]]; then
        echo -e "${RED}✗${NC} Aura harness not found at: ${AURA_HARNESS_PATH}"
        echo "Use --harness-path to specify the location"
        exit 1
    fi
    
    if [[ ! -f "${AURA_HARNESS_PATH}/Dockerfile" ]]; then
        echo -e "${RED}✗${NC} Dockerfile not found in aura-harness"
        echo "Expected: ${AURA_HARNESS_PATH}/Dockerfile"
        exit 1
    fi
    
    # Create ECR repo for aura-harness if it doesn't exist
    HARNESS_REPO_NAME="${RESOURCE_PREFIX}-harness"
    if ! aws ecr describe-repositories --repository-names "${HARNESS_REPO_NAME}" &> /dev/null; then
        echo "Creating ECR repository: ${HARNESS_REPO_NAME}"
        aws ecr create-repository \
            --repository-name "${HARNESS_REPO_NAME}" \
            --image-scanning-configuration scanOnPush=true
    fi
    
    HARNESS_IMAGE="${ECR_REGISTRY}/${HARNESS_REPO_NAME}:${IMAGE_TAG}"
    
    echo "Building aura-harness image..."
    docker build \
        --no-cache \
        -t "${HARNESS_REPO_NAME}:${IMAGE_TAG}" \
        -t "${HARNESS_IMAGE}" \
        "${AURA_HARNESS_PATH}"
    
    echo -e "${GREEN}✓${NC} Built ${HARNESS_REPO_NAME}:${IMAGE_TAG}"
    
    echo "Pushing to ECR..."
    docker push "${HARNESS_IMAGE}"
    
    echo -e "${GREEN}✓${NC} Pushed ${HARNESS_IMAGE}"
fi

#------------------------------------------------------------------------------
# Summary
#------------------------------------------------------------------------------

echo ""
echo -e "${GREEN}=============================================="
echo "  Build complete!"
echo "==============================================${NC}"
echo ""
echo "Images pushed:"

if [[ "$BUILD_PLATFORM" == "true" ]]; then
    for service in "${SERVICES[@]}"; do
        echo "  ${ECR_REGISTRY}/${RESOURCE_PREFIX}-${service}:${IMAGE_TAG}"
    done
fi

if [[ "$BUILD_HARNESS" == "true" ]]; then
    echo "  ${ECR_REGISTRY}/${RESOURCE_PREFIX}-harness:${IMAGE_TAG}"
fi

echo ""

#------------------------------------------------------------------------------
# Refresh Kubernetes deployments (optional)
#------------------------------------------------------------------------------

if [[ "$REFRESH_K8S" == "true" ]]; then
    echo "=============================================="
    echo "  Refreshing Kubernetes Deployments"
    echo "=============================================="
    echo ""
    
    # Determine which services to refresh
    REFRESH_SERVICES=()
    if [[ "$REFRESH_GATEWAY_ONLY" == "true" ]]; then
        REFRESH_SERVICES+=("gateway")
    elif [[ "$BUILD_PLATFORM" == "true" ]]; then
        REFRESH_SERVICES+=("gateway" "control" "scheduler")
    fi
    
    if [[ ${#REFRESH_SERVICES[@]} -gt 0 ]]; then
        echo "Restarting deployments to pull latest images..."
        echo ""
        
        for service in "${REFRESH_SERVICES[@]}"; do
            DEPLOYMENT="aura-swarm-${service}"
            echo "Restarting ${DEPLOYMENT}..."
            kubectl rollout restart deployment/${DEPLOYMENT} -n "${K8S_NAMESPACE_SYSTEM}" || true
        done
        
        echo ""
        echo "Waiting for rollouts to complete..."
        echo ""
        
        for service in "${REFRESH_SERVICES[@]}"; do
            DEPLOYMENT="aura-swarm-${service}"
            echo "Waiting for ${DEPLOYMENT}..."
            kubectl rollout status deployment/${DEPLOYMENT} -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s || true
        done
        
        echo ""
        echo "Current pod status:"
        kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -o wide
        echo ""
        echo -e "${GREEN}✓${NC} Kubernetes deployments refreshed"
    else
        echo "No platform services were built, skipping refresh."
    fi
    
    # Restart agent pods if harness was rebuilt
    if [[ "$BUILD_HARNESS" == "true" ]]; then
        echo ""
        echo "=============================================="
        echo "  Restarting Harness Agent Pods"
        echo "=============================================="
        echo ""
        
        AGENT_COUNT=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent --no-headers 2>/dev/null | wc -l || echo "0")
        
        if [[ "$AGENT_COUNT" -gt 0 ]]; then
            echo "Found ${AGENT_COUNT} running agent pod(s)."
            echo "Deleting all agent pods to pull new harness image..."
            echo ""
            
            kubectl delete pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent --wait=false
            
            echo ""
            echo -e "${GREEN}✓${NC} Agent pods deleted. They will be recreated with the new image when sessions are opened."
        else
            echo "No running agent pods found."
        fi
    fi
else
    echo "Next step: Run ./08-deploy-k8s.sh"
    echo ""
    echo "To refresh existing deployments after building:"
    echo "  ./refresh-k8s.sh"
    echo ""
    echo "Or use --refresh flag next time:"
    echo "  ./07-build-images.sh --dev-mode --refresh          # All services"
    echo "  ./07-build-images.sh --dev-mode --refresh-gateway  # Gateway only"
fi
