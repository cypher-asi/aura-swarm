#!/bin/bash
# 07-build-images.sh - Build and push container images
#
# Builds:
# - aura-swarm-gateway
# - aura-swarm-control
# - aura-swarm-scheduler
# - aura-runtime (optional, from ../aura-runtime)
#
# Pushes to ECR repositories
#
# Usage:
#   ./07-build-images.sh              # Build platform services only
#   ./07-build-images.sh --all        # Build platform + aura-runtime
#   ./07-build-images.sh --runtime    # Build aura-runtime only

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
BUILD_RUNTIME=false
DEV_MODE=false
REFRESH_K8S=false
REFRESH_GATEWAY_ONLY=false
AURA_RUNTIME_PATH="${PROJECT_ROOT}/../aura-runtime"

while [[ $# -gt 0 ]]; do
    case $1 in
        --all)
            BUILD_RUNTIME=true
            shift
            ;;
        --runtime)
            BUILD_PLATFORM=false
            BUILD_RUNTIME=true
            shift
            ;;
        --runtime-path)
            AURA_RUNTIME_PATH="$2"
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
            echo "Usage: $0 [--all] [--runtime] [--runtime-path PATH] [--dev-mode] [--refresh] [--refresh-gateway]"
            exit 1
            ;;
    esac
done

echo "=============================================="
echo "  Aura Swarm - Build and Push Images"
echo "=============================================="
echo ""
echo "Build platform services: ${BUILD_PLATFORM}"
echo "Build aura-runtime: ${BUILD_RUNTIME}"
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
            -f - "${PROJECT_ROOT}" <<'EOF'
# Build stage - compile Rust binary
FROM rust:1.85-bookworm AS builder

# Install build dependencies for native crates (RocksDB, zstd, etc.)
RUN apt-get update && apt-get install -y \
    build-essential \
    clang \
    libclang-dev \
    llvm-dev \
    pkg-config \
    libssl-dev \
    cmake \
    && rm -rf /var/lib/apt/lists/*

ARG SERVICE
ARG CARGO_FEATURES=""

WORKDIR /build

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Pin crates to versions compatible with Rust 1.85
RUN cargo update time --precise 0.3.36 && \
    cargo update time-core --precise 0.1.2 && \
    cargo update time-macros --precise 0.2.18 && \
    cargo update home --precise 0.5.9

# Build release binary (with optional features)
RUN cargo build --release --bin aura-swarm-${SERVICE} ${CARGO_FEATURES}

# Runtime stage - minimal image
FROM debian:bookworm-slim

ARG SERVICE

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash aura

WORKDIR /app

COPY --from=builder /build/target/release/aura-swarm-${SERVICE} /usr/local/bin/aura-swarm-${SERVICE}

# Create symlink so entrypoint works regardless of service name
RUN ln -s /usr/local/bin/aura-swarm-${SERVICE} /usr/local/bin/service

USER aura

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/service"]
EOF
        
        echo -e "${GREEN}✓${NC} Built ${IMAGE_NAME}:${IMAGE_TAG}"
        
        echo "Pushing to ECR..."
        docker push "${FULL_IMAGE}"
        
        echo -e "${GREEN}✓${NC} Pushed ${FULL_IMAGE}"
    done
fi

#------------------------------------------------------------------------------
# Build aura-runtime
#------------------------------------------------------------------------------

if [[ "$BUILD_RUNTIME" == "true" ]]; then
    echo ""
    echo "=============================================="
    echo "  Building Aura Runtime"
    echo "=============================================="
    echo ""
    
    if [[ ! -d "${AURA_RUNTIME_PATH}" ]]; then
        echo -e "${RED}✗${NC} Aura runtime not found at: ${AURA_RUNTIME_PATH}"
        echo "Use --runtime-path to specify the location"
        exit 1
    fi
    
    if [[ ! -f "${AURA_RUNTIME_PATH}/Dockerfile" ]]; then
        echo -e "${RED}✗${NC} Dockerfile not found in aura-runtime"
        echo "Expected: ${AURA_RUNTIME_PATH}/Dockerfile"
        exit 1
    fi
    
    # Create ECR repo for aura-runtime if it doesn't exist
    RUNTIME_REPO_NAME="${RESOURCE_PREFIX}-runtime"
    if ! aws ecr describe-repositories --repository-names "${RUNTIME_REPO_NAME}" &> /dev/null; then
        echo "Creating ECR repository: ${RUNTIME_REPO_NAME}"
        aws ecr create-repository \
            --repository-name "${RUNTIME_REPO_NAME}" \
            --image-scanning-configuration scanOnPush=true
    fi
    
    RUNTIME_IMAGE="${ECR_REGISTRY}/${RUNTIME_REPO_NAME}:${IMAGE_TAG}"
    
    echo "Building aura-runtime image..."
    docker build \
        -t "${RUNTIME_REPO_NAME}:${IMAGE_TAG}" \
        -t "${RUNTIME_IMAGE}" \
        "${AURA_RUNTIME_PATH}"
    
    echo -e "${GREEN}✓${NC} Built ${RUNTIME_REPO_NAME}:${IMAGE_TAG}"
    
    echo "Pushing to ECR..."
    docker push "${RUNTIME_IMAGE}"
    
    echo -e "${GREEN}✓${NC} Pushed ${RUNTIME_IMAGE}"
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

if [[ "$BUILD_RUNTIME" == "true" ]]; then
    echo "  ${ECR_REGISTRY}/${RESOURCE_PREFIX}-runtime:${IMAGE_TAG}"
    echo ""
    echo -e "${YELLOW}Note: Update scheduler config to use:${NC}"
    echo "  AURA_RUNTIME_IMAGE=${ECR_REGISTRY}/${RESOURCE_PREFIX}-runtime:${IMAGE_TAG}"
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
