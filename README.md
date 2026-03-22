<p align="center">
  <!-- <img src="assets/banner.png" alt="AURA SWARM" width="480" /> -->
  <strong style="font-size: 2em;">AURA SWARM</strong>
</p>

---

<p align="center">
  <strong>Isolated AI Agents in MicroVMs</strong><br/>
  A multi-user platform for running sandboxed AI agents on Firecracker microVMs, orchestrated by Kubernetes and secured by zOS authentication.
</p>

<p align="center">
  <a href="#overview">Overview</a> &nbsp;·&nbsp;
  <a href="#quick-start">Quick Start</a> &nbsp;·&nbsp;
  <a href="#architecture">Architecture</a> &nbsp;·&nbsp;
  <a href="#principles">Principles</a> &nbsp;·&nbsp;
  <a href="docs/spec/v0.1.0/README.md">Spec</a>
</p>

## Overview

Aura Swarm is a platform for running isolated AI agents as long-lived services. Each agent runs inside its own Firecracker microVM (via Kata Containers on Kubernetes), giving every user a dedicated, sandboxed execution environment with persistent state.

Users interact with their agents through WebSocket sessions proxied by the gateway, or through the `aswarm` terminal UI. The platform handles the full agent lifecycle — provisioning, running, hibernation, wake-on-demand, and teardown — while keeping per-user state isolated under `/state/<user_id>/<agent_id>/`.

Authentication is handled via zOS (JWT/JWKS), and usage is tracked through z-billing for credit-based access to LLM-powered agents.

---

<p align="center">
  <strong style="font-size: 1.5em;">Core Concepts</strong>
</p>

1. **Agents:** Long-running Aura runtime instances, each owned by a single user. An agent is backed by a Kubernetes pod running in a Firecracker microVM with its own filesystem, network identity, and lifecycle state.

2. **Sessions:** Interactive WebSocket connections between a user and their agent. The gateway authenticates the user, locates the agent pod, and proxies traffic bidirectionally in real time.

3. **MicroVMs:** Every agent pod runs under the Kata runtime class, providing kernel-level isolation via Firecracker. No agent can access another agent's memory, filesystem, or network namespace.

4. **Lifecycle:** Agents transition through well-defined states — Provisioning, Running, Idle, Hibernating, Stopping, Stopped, and Error — managed by the control plane and reconciled by the scheduler.

### Quick Start

```sh
git clone https://github.com/cypher-asi/aura-swarm && cd aura-swarm
cargo build
```

Run the gateway locally:

```sh
DATA_DIR=.data LISTEN_ADDR=0.0.0.0:8080 cargo run -p aura-swarm-gateway
```

Connect with the CLI:

```sh
export AURA_SWARM_GATEWAY=http://localhost:8080
export AURA_SWARM_TOKEN=<your-jwt>
cargo run -p aura-swarm-cli
```

See the [full specification](docs/spec/v0.1.0/README.md) for production deployment and component details.

## Principles

1. **Isolation:** Every agent runs in its own microVM. No shared kernel, no shared filesystem, no lateral movement. Isolation is the default, not an option.
2. **Stateful:** Agent state persists across restarts and hibernation. Users pick up where they left off — the platform manages the storage lifecycle transparently.
3. **API-First:** A clean REST + WebSocket API fronts all operations. The gateway is the single entry point; internal services are never exposed publicly.
4. **Observable:** Structured tracing throughout every crate. Every request, lifecycle transition, and pod event is logged with correlation IDs.
5. **Secure by Default:** JWT validation on every request, RBAC in Kubernetes, network policies between namespaces, and `unsafe` code is forbidden workspace-wide.

## Architecture

| Crate | Description |
|---|---|
| `aura-swarm-core` | Shared types, strongly-typed IDs (`AgentId`, `UserId`, `SessionId`), and error definitions |
| `aura-swarm-store` | RocksDB storage: agents, sessions, users, and secondary indexes with CBOR serialization |
| `aura-swarm-auth` | zOS integration: JWKS fetching, JWT validation |
| `aura-swarm-control` | Agent lifecycle management, session CRUD, and scheduler orchestration |
| `aura-swarm-scheduler` | Kubernetes reconciler: pod creation, health monitoring, status callbacks, and resource limits |
| `aura-swarm-gateway` | Public Axum HTTP/WebSocket API, auth middleware, billing middleware, and WebSocket proxy |
| `aura-swarm-cli` | Terminal UI (`aswarm`): agent listing, creation, and interactive chat via ratatui |

## Project Structure

```
aura-swarm/
  Cargo.toml                    # workspace root
  crates/
    aura-swarm-core/            # IDs, schemas, errors
    aura-swarm-store/           # RocksDB storage backend
    aura-swarm-auth/            # JWT / JWKS authentication
    aura-swarm-control/         # agent lifecycle, sessions
    aura-swarm-scheduler/       # K8s pod reconciler
    aura-swarm-gateway/         # public HTTP + WebSocket API
    aura-swarm-cli/             # terminal UI (aswarm)
  docker/
    Dockerfile.gateway          # gateway container image
    Dockerfile.control          # control plane container image
    Dockerfile.scheduler        # scheduler container image
  deploy/
    config.env                  # AWS / EKS configuration
    k8s/                        # Kubernetes manifests
  docs/
    spec/v0.1.0/                # platform specification
```

## Deployment

Container images are built with multi-stage Dockerfiles under `docker/` (base: `rust:1.85-bookworm`, runtime: `debian:bookworm-slim`). Production deployment targets AWS EKS with Kata Containers for microVM isolation.

The `deploy/` directory contains Terraform modules for VPC, EKS, EFS, and ECR, plus numbered shell scripts that provision infrastructure end-to-end. Kubernetes manifests under `deploy/k8s/` define namespaces, RBAC, secrets, deployments, services, and network policies.

See `deploy/config.env` for configurable parameters and `deploy/k8s/03-secrets.yaml` for required secret references.

## License

MIT
