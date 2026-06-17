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
#   ./07-build-images.sh --harness-path PATH --aura-os-path PATH

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"
HARNESS_STATE_FILE="${SCRIPT_DIR}/.last-harness-image.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Force plain, line-streamed BuildKit output so long `docker build` runs show
# real-time progress instead of an interactive TUI that looks frozen in many
# terminals (Docker 29.x defaults to --progress=auto).
export BUILDKIT_PROGRESS="${BUILDKIT_PROGRESS:-plain}"

# Parse arguments
BUILD_PLATFORM=true
DEV_MODE=false
REFRESH_K8S=false
REFRESH_GATEWAY_ONLY=false
AURA_HARNESS_PATH="${PROJECT_ROOT}/../aura-harness"
AURA_OS_PATH=""
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
        --aura-os-path)
            AURA_OS_PATH="$2"
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
            echo "Usage: $0 [--all] [--harness] [--harness-path PATH] [--aura-os-path PATH] [--dev-mode] [--refresh] [--refresh-gateway]"
            exit 1
            ;;
    esac
done

echo "=============================================="
echo "  AURA Swarm - Build and Push Images"
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

#------------------------------------------------------------------------------
# Preflight: verify credentials BEFORE doing a long build.
#
# Build + push can take many minutes. AWS session tokens (esp. STS / SSO)
# routinely expire mid-script, which historically left us with a pushed
# image but a failed scheduler restart. Fail fast instead.
#------------------------------------------------------------------------------

echo "Running preflight checks..."

# Docker must be up before anything else. `docker info` blocks indefinitely
# when the daemon is mid-startup or the pipe is unreachable, so wrap it in a
# timeout to FAIL FAST instead of stalling the whole script. Falls back to a
# plain check when `timeout` is unavailable on PATH.
DOCKER_INFO_TIMEOUT=15
if command -v timeout &>/dev/null; then
    docker_ready() { timeout "${DOCKER_INFO_TIMEOUT}" docker info >/dev/null 2>&1; }
else
    docker_ready() { docker info >/dev/null 2>&1; }
fi
if ! docker_ready; then
    echo -e "${RED}✗${NC} Docker is not running (or not responding within ${DOCKER_INFO_TIMEOUT}s)."
    echo ""
    echo "  Start Docker Desktop, wait until it reports 'running', then re-run this script."
    exit 1
fi
echo -e "${GREEN}✓${NC} Docker daemon is responding"

if ! AWS_CALLER_ARN=$(aws sts get-caller-identity --output text --query Arn 2>&1); then
    echo -e "${RED}✗${NC} AWS credentials are missing or expired."
    echo "  aws sts get-caller-identity said:"
    echo "    ${AWS_CALLER_ARN}"
    echo ""
    echo "  Refresh your AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY /"
    echo "  AWS_SESSION_TOKEN env vars (or your SSO session) and re-run."
    exit 1
fi
echo -e "${GREEN}✓${NC} AWS identity: ${AWS_CALLER_ARN}"

if [[ "$REFRESH_K8S" == "true" ]]; then
    if ! command -v kubectl &>/dev/null; then
        echo -e "${RED}✗${NC} kubectl not found on PATH but --refresh was requested."
        exit 1
    fi
    if ! kubectl auth can-i get deployments -n "${K8S_NAMESPACE_SYSTEM}" &>/dev/null; then
        echo -e "${RED}✗${NC} kubectl cannot authenticate to the cluster."
        echo "  EKS uses the active AWS identity to mint a token; if AWS creds"
        echo "  are fine but this still fails, your kubeconfig is stale."
        echo ""
        echo "  Refresh with:"
        echo "    aws eks update-kubeconfig --region ${AWS_REGION} --name ${EKS_CLUSTER_NAME}"
        exit 1
    fi
    echo -e "${GREEN}✓${NC} kubectl authenticated to ${EKS_CLUSTER_NAME}"
fi

echo ""

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
            --progress=plain \
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

    # The harness workspace is now self-contained: its Dockerfile copies
    # only ./Cargo.toml, ./src, and ./crates (which includes its own
    # aura-protocol crate). The build context is therefore the harness
    # directory itself — no sibling aura-os checkout is required anymore.
    # (--aura-os-path is accepted but ignored for backward compatibility.)
    if [[ -n "${AURA_OS_PATH}" ]]; then
        echo -e "${YELLOW}⚠${NC} --aura-os-path is deprecated and ignored: the harness build is self-contained."
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

    # Bake the harness git commit into the image so the /health endpoint
    # (and the aura-os env popup) can report which commit a pod is running.
    HARNESS_GIT_SHA=$(git -C "${AURA_HARNESS_PATH}" rev-parse HEAD 2>/dev/null || echo "")
    if [[ -z "${HARNESS_GIT_SHA}" ]]; then
        echo -e "${YELLOW}⚠${NC} Could not resolve git commit for ${AURA_HARNESS_PATH}; image will report no git_sha"
    else
        echo "Harness git commit: ${HARNESS_GIT_SHA}"
        if [[ -n "$(git -C "${AURA_HARNESS_PATH}" status --porcelain 2>/dev/null)" ]]; then
            echo -e "${YELLOW}⚠${NC} aura-harness working tree is dirty; baked git_sha may not match image contents"
        fi
    fi

    echo "Building aura-harness image..."
    docker build \
        --progress=plain \
        --no-cache \
        --build-arg GIT_SHA="${HARNESS_GIT_SHA}" \
        -f "${AURA_HARNESS_PATH}/Dockerfile" \
        -t "${HARNESS_REPO_NAME}:${IMAGE_TAG}" \
        -t "${HARNESS_IMAGE}" \
        "${AURA_HARNESS_PATH}"
    
    echo -e "${GREEN}✓${NC} Built ${HARNESS_REPO_NAME}:${IMAGE_TAG}"
    
    echo "Pushing to ECR..."
    docker push "${HARNESS_IMAGE}"
    
    echo -e "${GREEN}✓${NC} Pushed ${HARNESS_IMAGE}"

    echo "Resolving immutable harness digest..."
    HARNESS_DIGEST=$(aws ecr describe-images \
        --repository-name "${HARNESS_REPO_NAME}" \
        --image-ids imageTag="${IMAGE_TAG}" \
        --query 'imageDetails[0].imageDigest' \
        --output text 2>/dev/null || echo "")

    if [[ -n "${HARNESS_DIGEST}" && "${HARNESS_DIGEST}" != "None" ]]; then
        HARNESS_PINNED_IMAGE="${ECR_REGISTRY}/${HARNESS_REPO_NAME}@${HARNESS_DIGEST}"
        {
            echo "# Auto-generated by 07-build-images.sh"
            echo "AURA_HARNESS_IMAGE=${HARNESS_PINNED_IMAGE}"
            echo "AURA_HARNESS_TAGGED_IMAGE=${HARNESS_IMAGE}"
            echo "AURA_HARNESS_DIGEST=${HARNESS_DIGEST}"
            echo "AURA_HARNESS_GIT_SHA=${HARNESS_GIT_SHA}"
        } > "${HARNESS_STATE_FILE}"
        echo -e "${GREEN}✓${NC} Resolved digest: ${HARNESS_DIGEST}"
        echo -e "${GREEN}✓${NC} Persisted deploy state: ${HARNESS_STATE_FILE}"
    else
        echo -e "${RED}✗${NC} Unable to resolve digest for ${HARNESS_IMAGE}"
        echo "  Refusing to continue because redeploy verification requires an immutable harness digest."
        exit 1
    fi
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
    if [[ -f "${HARNESS_STATE_FILE}" ]]; then
        echo "  (digest persisted in ${HARNESS_STATE_FILE})"
    fi
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
            kubectl rollout restart deployment/${DEPLOYMENT} -n "${K8S_NAMESPACE_SYSTEM}"
        done
        
        echo ""
        echo "Waiting for rollouts to complete..."
        echo ""
        
        for service in "${REFRESH_SERVICES[@]}"; do
            DEPLOYMENT="aura-swarm-${service}"
            echo "Waiting for ${DEPLOYMENT}..."
            kubectl rollout status deployment/${DEPLOYMENT} -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s
        done
        
        echo ""
        echo "Current pod status:"
        kubectl get pods -n "${K8S_NAMESPACE_SYSTEM}" -o wide
        echo ""
        echo -e "${GREEN}✓${NC} Kubernetes deployments refreshed"
    else
        echo "No platform services were built, skipping refresh."
    fi
    
    # Converge agent pods if harness was rebuilt
    if [[ "$BUILD_HARNESS" == "true" ]]; then
        echo ""
        echo "=============================================="
        echo "  Agent Pod Convergence"
        echo "=============================================="
        echo ""
        
        # Restart the scheduler so it picks up the new AURA_HARNESS_IMAGE.
        # Its desired-state reconciler will detect image drift on existing
        # agent pods and rolling-replace them automatically.
        echo "Restarting scheduler to pick up new harness image..."
        kubectl rollout restart deployment/aura-swarm-scheduler -n "${K8S_NAMESPACE_SYSTEM}"
        kubectl rollout status deployment/aura-swarm-scheduler -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s
        
        AGENT_POD_COUNT=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent \
            --no-headers 2>/dev/null | wc -l | tr -d ' ' || echo "0")
        
        if [[ "${AGENT_POD_COUNT}" -gt 0 ]]; then
            echo ""
            echo -e "${GREEN}✓${NC} ${AGENT_POD_COUNT} agent pod(s) running. The scheduler reconciler"
            echo "  will rolling-replace pods with the old harness image (~30s cycle)."
        else
            echo ""
            echo "No running agent pods. New agents will use the updated harness image."
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
