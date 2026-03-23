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
