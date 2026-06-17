<p align="center">
  <!-- <img src="assets/banner.png" alt="AURA SWARM" width="480" /> -->
  <strong style="font-size: 2em;">AURA SWARM</strong>
</p>

---

<p align="center">
  <strong>Confidential AI Agents in SEV-SNP TEEs</strong><br/>
  A multi-user platform for running AI agents in confidential virtual machines — hardware-encrypted memory, attestation-gated sealed storage, and per-tier hourly pricing — orchestrated by Kubernetes and secured by zOS authentication.
</p>

<p align="center">
  <a href="#overview">Overview</a> &nbsp;·&nbsp;
  <a href="#quick-start">Quick Start</a> &nbsp;·&nbsp;
  <a href="#architecture">Architecture</a> &nbsp;·&nbsp;
  <a href="#principles">Principles</a> &nbsp;·&nbsp;
  <a href="docs/spec/v0.2.0/README.md">Spec</a>
</p>

## Overview

AURA Swarm is a platform for running isolated AI agents as long-lived services. **Every agent in the fleet is a confidential SEV-SNP virtual machine** (Confidential Containers: Kata + QEMU via the `kata-qemu-snp` runtime class on AMD bare-metal nodes). Agent memory is hardware-encrypted, agent state is sealed at rest with a per-agent key released only after remote attestation by a Trustee KBS, and the platform itself — control plane, hosts, storage, operators — sits outside the trust boundary for agent data. The migration to the all-TEE architecture is complete: there is no legacy microVM tier.

Users interact with their agents through run streams proxied by the gateway, or through the `aswarm` terminal UI. The platform handles the full agent lifecycle — provisioning, running, hibernation, three wake paths (API, new session, cron trigger), tier resizing, and crypto-erasing teardown — while keeping per-agent sealed state isolated on encrypted EFS.

Authentication is handled via zOS (JWT/JWKS), and usage is billed through the zbilling service at fixed per-tier hourly rates.

### Box Tiers

| Tier | CPU | Memory | Price | Billing SKU |
|---|---|---|---|---|
| `small` | 500m | 1 GiB | 4¢/hour | `swarm.small` |
| `standard` (default) | 1000m | 2 GiB | 8¢/hour | `swarm.standard` |
| `pro` | 2000m | 4 GiB | 15¢/hour | `swarm.pro` |

All tiers share identical isolation, attestation, and sealing — a tier change (`POST /v1/agents/:id/tier`) is purely a resize. Hibernated agents cost nothing.

---

<p align="center">
  <strong style="font-size: 1.5em;">Core Concepts</strong>
</p>

1. **Agents:** Long-running aura-harness instances, each owned by a single user. An agent is backed by a Kubernetes pod running in a SEV-SNP confidential VM with its own encrypted memory, sealed filesystem state, network identity, and lifecycle.

2. **Sealed storage:** Every content-bearing value the agent persists (conversation record, memory, secrets, process definitions) is AES-256-GCM encrypted under a per-agent DEK before it reaches disk. The DEK lives in the Trustee KBS and is released only into an attested guest; deleting the agent revokes the key (crypto-erase). EFS and backups only ever hold ciphertext.

3. **Secrets vault & processes:** An in-TEE secrets vault (gateway pass-through API, no control-plane persistence) and in-TEE cron automations — the platform stores only `(process_id, cron, enabled, next_run_at)` and fires content-free triggers; prompts, config, and run data never leave the VM.

4. **Sessions & runs:** Interactive WebSocket run streams between a user and their agent, authenticated and proxied by the gateway.

5. **Lifecycle:** Provisioning → Running ⇄ Idle → Hibernating/Stopped → wake-on-demand (API call, new session, or cron trigger), with auto-hibernate after idle. Usage events with at-event-time prices power per-agent and per-user cost stats (`GET /v1/usage`).

### Quick Start

```sh
git clone https://github.com/cypher-asi/aura-swarm && cd aura-swarm
cargo build
```

Run the gateway locally (dev mode: container isolation, no KBS — sealed mode needs the deployed stack):

```sh
DATA_DIR=.data LISTEN_ADDR=0.0.0.0:8080 cargo run -p aura-swarm-gateway
```

Connect with the CLI:

```sh
export AURA_SWARM_GATEWAY=http://localhost:8080
export AURA_SWARM_TOKEN=<your-jwt>
cargo run -p aura-swarm-cli
```

See the [full specification](docs/spec/v0.2.0/README.md) for production deployment and component details, and [deploy/README.md](deploy/README.md) for the confidential-infrastructure install order (SNP node group, CoCo operator, Trustee KBS).

## Principles

1. **Confidential by default:** Every agent runs in a SEV-SNP TEE. The host kernel, the hypervisor, and the platform operators cannot read agent memory or state. Isolation is hardware-enforced, not a configuration option.
2. **Trigger outside, data inside:** Anything that must leave the TEE for the platform to function (schedules, lifecycle state, usage counters) is content-free metadata. Everything with content stays sealed inside.
3. **Stateful:** Sealed agent state persists across restarts, hibernation, and tier changes. Users pick up where they left off — and destroying an agent crypto-erases it.
4. **API-First:** A clean REST + WebSocket API fronts all operations. The gateway is the single entry point; internal services authenticate with a shared service token and are never exposed publicly.
5. **Observable within the boundary:** Structured tracing throughout every crate; per-VM platform logs (live tail + termination snapshots) and usage/cost stats — without ever collecting in-VM agent content.

## Architecture

| Crate | Description |
|---|---|
| `aura-swarm-core` | Shared types, strongly-typed IDs (`AgentId`, `UserId`, `SessionId`), and error definitions |
| `aura-swarm-store` | RocksDB storage: agents (tier + sealed-storage spec), sessions, users, process triggers, usage events, log snapshots; startup schema migrations |
| `aura-swarm-auth` | zOS integration: JWKS fetching, JWT validation |
| `aura-swarm-control` | Agent lifecycle, tier changes, sessions, Trustee KBS DEK lifecycle, ProcessCronService + auto-hibernate, usage aggregation |
| `aura-swarm-scheduler` | Kubernetes reconciler: `kata-qemu-snp` pod builder (SNP node selection, KBS/sealing env), health monitoring, pod-log snapshots, tier-SKU billing reporter |
| `aura-swarm-gateway` | Public Axum HTTP/WebSocket API, auth middleware, run/terminal/file/secrets proxying, internal service API |
| `aura-swarm-client` | Typed client for external consumers (aura-os, aura-network) |
| `aura-swarm-cli` | Terminal UI (`aswarm`): agent listing, creation, and interactive chat via ratatui |
| `aura-swarm-deploy` | Deploy TUI (`aswarm-deploy`): runs the staged `deploy/` scripts with a curated step-progress column and a live machine view (raw output + read-only infra-state snapshots) via ratatui |

The agent runtime inside the TEE is **aura-harness** (sibling repository): attestation boot, sealed stores, secrets vault, processes, run/terminal/file APIs.

## Project Structure

```
aura-swarm/
  Cargo.toml                    # workspace root
  crates/
    aura-swarm-core/            # IDs, schemas, errors
    aura-swarm-store/           # RocksDB storage + migrations
    aura-swarm-auth/            # JWT / JWKS authentication
    aura-swarm-control/         # lifecycle, sessions, cron, DEK, usage
    aura-swarm-scheduler/       # K8s pod reconciler + billing
    aura-swarm-gateway/         # public HTTP + WebSocket API
    aura-swarm-client/          # typed external client
    aura-swarm-cli/             # terminal UI (aswarm)
    aura-swarm-deploy/          # deploy-script runner TUI (aswarm-deploy)
  docker/
    Dockerfile.gateway          # gateway container image
    Dockerfile.control          # control plane container image
    Dockerfile.scheduler        # scheduler container image
  deploy/
    config.env                  # AWS / EKS / SNP-pool configuration
    k8s/                        # K8s manifests (incl. CoCo CcRuntime, Trustee, runtime classes)
    terraform/                  # VPC, EKS (+ SNP bare-metal node group), EFS, ECR
  docs/
    spec/v0.2.0/                # current platform specification
    spec/v0.1.0/                # superseded (microVM era)
```

## Deployment

Container images are built with multi-stage Dockerfiles under `docker/`. Production targets AWS EKS with two node pools: an untainted system pool (gateway, scheduler, Trustee KBS) and a tainted **SEV-SNP bare-metal pool** (default `m6a.metal`, labeled `swarm.io/confidential-node=true`) where the Confidential Containers operator installs the `kata-qemu-snp` runtime for agent pods. EFS encryption is mandatory.

The `deploy/` directory contains Terraform modules plus a numbered shell-script sequence (00–12) implementing the staged TEE rollout — preflight, deploy-operator IAM, SNP node group, CoCo operator, Trustee KBS (with admin keypair generation), attestation smoke test, R1 deploy + soak, EFS backup, R2 migration + convergence gate, R3 cleanup, and finalize — each step verifying itself before handing off. See [deploy/README.md](deploy/README.md) for the step table and soak guidance; the pre-TEE-rollout scripts are kept in `deploy/legacy/`.

## License

MIT
