#!/bin/bash
# 07-build-images.sh - Build and push container images
#
# Builds:
# - aura-swarm-gateway
# - aura-swarm-control
# - aura-swarm-scheduler
#
# Pushes to ECR repositories

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Build and Push Images"
echo "=============================================="
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
# Build Rust binaries
#------------------------------------------------------------------------------

echo "Building Rust binaries (release mode)..."
echo ""

cargo build --release

echo ""
echo -e "${GREEN}✓${NC} Rust binaries built"
echo ""

#------------------------------------------------------------------------------
# Create Dockerfile if not exists
#------------------------------------------------------------------------------

DOCKERFILE="${PROJECT_ROOT}/Dockerfile"

if [[ ! -f "${DOCKERFILE}" ]]; then
    echo "Creating Dockerfile..."
    
    cat > "${DOCKERFILE}" <<'EOF'
# Multi-stage build for aura-swarm services
FROM rust:1.75-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy source code
COPY . .

# Build release binaries
RUN cargo build --release

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 -s /bin/bash aura

WORKDIR /app

# Copy binaries from builder
COPY --from=builder /app/target/release/aura-swarm-gateway /usr/local/bin/
COPY --from=builder /app/target/release/aura-swarm-control /usr/local/bin/
COPY --from=builder /app/target/release/aura-swarm-scheduler /usr/local/bin/

USER aura

# Default to gateway, override with CMD
ENTRYPOINT ["/usr/local/bin/aura-swarm-gateway"]
EOF
    
    echo -e "${GREEN}✓${NC} Dockerfile created"
fi

#------------------------------------------------------------------------------
# Build and push images
#------------------------------------------------------------------------------

# Services to build
SERVICES=("gateway" "control" "scheduler")

for service in "${SERVICES[@]}"; do
    IMAGE_NAME="${RESOURCE_PREFIX}-${service}"
    FULL_IMAGE="${ECR_REGISTRY}/${IMAGE_NAME}:${IMAGE_TAG}"
    
    echo ""
    echo "Building ${service} image..."
    
    # Build with service-specific entrypoint
    docker build \
        --build-arg SERVICE="${service}" \
        -t "${IMAGE_NAME}:${IMAGE_TAG}" \
        -t "${FULL_IMAGE}" \
        -f - "${PROJECT_ROOT}" <<EOF
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash aura

WORKDIR /app

COPY target/release/aura-swarm-${service} /usr/local/bin/aura-swarm-${service}

USER aura

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/aura-swarm-${service}"]
EOF
    
    echo -e "${GREEN}✓${NC} Built ${IMAGE_NAME}:${IMAGE_TAG}"
    
    echo "Pushing to ECR..."
    docker push "${FULL_IMAGE}"
    
    echo -e "${GREEN}✓${NC} Pushed ${FULL_IMAGE}"
done

echo ""
echo -e "${GREEN}=============================================="
echo "  All images built and pushed!"
echo "==============================================${NC}"
echo ""
echo "Images:"
for service in "${SERVICES[@]}"; do
    echo "  ${ECR_REGISTRY}/${RESOURCE_PREFIX}-${service}:${IMAGE_TAG}"
done
echo ""
echo "Next step: Run ./08-deploy-k8s.sh"
