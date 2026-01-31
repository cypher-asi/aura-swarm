# System Overview — Specification v0.1.0

## 1. Overview

The MicroVM Agent Platform is a multi-user system for running isolated AI agents. Each agent runs in its own microVM (Firecracker-backed via Kata Containers), providing strong security boundaries between users and their workloads.

### 1.1 Purpose

- Allow users to create, manage, and interact with AI agents
- Provide kernel-level isolation between agents using microVMs
- Support long-running background agents with persistent state
- Enable real-time interaction via WebSocket streaming

### 1.2 Core Principles

| Principle | Description |
|-----------|-------------|
| **MicroVM-first isolation** | Every agent runs in its own Firecracker microVM |
| **Stable IDs over ephemeral infra** | Agents addressed by `agent_id`, never by pod IP |
| **User isolation by design** | Data and network boundaries prevent cross-user access |
| **No public agent endpoints** | All access flows through the gateway |
| **Rust everywhere** | All platform services written in safe Rust |

### 1.3 Non-Goals (v0.1.0)

- Multi-tenant namespaces and policies (deferred)
- Cross-user agent communication
- Public per-agent DNS/IPs
- Exposing Kubernetes APIs to users
- Arbitrary user-supplied code (agents run Aura runtime only)

---

## 2. High-Level Architecture

### 2.1 System Layers

```mermaid
graph TB
    subgraph users [Users]
        Browser[Web Browser]
        CLI[CLI Client]
    end
    
    subgraph public [Public Layer]
        Gateway[aura-swarm-gateway<br/>HTTP + WebSocket]
    end
    
    subgraph control [Control Plane]
        Control[aura-swarm-control<br/>Agent Lifecycle]
        Store[(aura-swarm-store<br/>RocksDB)]
        Scheduler[aura-swarm-scheduler<br/>K8s Reconciler]
    end
    
    subgraph external [External Services]
        ZeroID[Zero-ID Server]
        EFS[EFS Storage]
    end
    
    subgraph execution [Execution Plane - Kubernetes]
        Node1[Node + Kata Runtime]
        Node2[Node + Kata Runtime]
        
        subgraph pods1 [MicroVM Pods]
            Agent1[Agent MicroVM<br/>Aura Runtime]
            Agent2[Agent MicroVM<br/>Aura Runtime]
        end
        
        subgraph pods2 [MicroVM Pods]
            Agent3[Agent MicroVM<br/>Aura Runtime]
        end
    end
    
    Browser --> Gateway
    CLI --> Gateway
    Gateway --> ZeroID
    Gateway --> Control
    Control --> Store
    Control --> Scheduler
    Scheduler --> Node1
    Scheduler --> Node2
    Node1 --> pods1
    Node2 --> pods2
    Agent1 --> EFS
    Agent2 --> EFS
    Agent3 --> EFS
```

### 2.2 Component Summary

| Component | Responsibility |
|-----------|---------------|
| **aura-swarm-gateway** | Public API, WebSocket proxy, JWT validation |
| **aura-swarm-control** | Agent CRUD, lifecycle management, session routing |
| **aura-swarm-store** | RocksDB persistence for agents, users, sessions |
| **aura-swarm-scheduler** | Kubernetes reconciler, pod lifecycle, health monitoring |
| **aura-swarm-auth** | Zero-ID integration, token validation |
| **Aura Runtime** | AI agent execution inside microVM |

---

## 3. Trust Boundaries

### 3.1 Boundary Diagram

```mermaid
graph LR
    subgraph untrusted [Untrusted]
        User[User Browser/CLI]
    end
    
    subgraph dmz [DMZ - Public API]
        Gateway[Gateway]
    end
    
    subgraph trusted [Trusted - Internal]
        Control[Control Plane]
        Store[(RocksDB)]
        Scheduler[Scheduler]
    end
    
    subgraph isolated [Isolated - Per Agent]
        VM1[MicroVM 1]
        VM2[MicroVM 2]
    end
    
    User -->|HTTPS + JWT| Gateway
    Gateway -->|Internal gRPC/HTTP| Control
    Control --> Store
    Control --> Scheduler
    Scheduler -->|K8s API| VM1
    Scheduler -->|K8s API| VM2
    Gateway -.->|WebSocket Proxy| VM1
    Gateway -.->|WebSocket Proxy| VM2
```

### 3.2 Trust Levels

| Zone | Components | Trust Level |
|------|------------|-------------|
| **Untrusted** | User browsers, CLI clients | None — all input validated |
| **DMZ** | aura-swarm-gateway | Authenticated — validates JWTs |
| **Trusted** | Control plane services | Full — internal network only |
| **Isolated** | Agent microVMs | Sandboxed — own kernel, no cross-VM access |

### 3.3 Security Boundaries

1. **Authentication Boundary**: Gateway validates JWT before any operation
2. **Authorization Boundary**: Control plane checks `user_id` ownership on every agent operation
3. **Network Boundary**: Agent pods have no public IPs; ingress only from control plane
4. **Kernel Boundary**: Each agent runs in separate Firecracker VM with own kernel
5. **Storage Boundary**: Agents only access their own `/state/<user_id>/<agent_id>/` path

---

## 4. Data Flow

### 4.1 Agent Creation Flow

```mermaid
sequenceDiagram
    participant User
    participant Gateway
    participant ZeroID
    participant Control
    participant Store
    participant Scheduler
    participant K8s
    
    User->>Gateway: POST /v1/agents (JWT)
    Gateway->>ZeroID: Validate JWT
    ZeroID-->>Gateway: user_id, email
    Gateway->>Control: CreateAgent(user_id, spec)
    Control->>Control: Generate agent_id
    Control->>Store: Insert agent record
    Control->>Scheduler: ScheduleAgent(agent_id)
    Scheduler->>K8s: Create Pod (RuntimeClass: kata-fc)
    K8s-->>Scheduler: Pod scheduled
    Scheduler->>Store: Update status = provisioning
    
    Note over K8s: Pod starts, Aura initializes
    
    K8s-->>Scheduler: Pod ready
    Scheduler->>Store: Update status = running
    Control-->>Gateway: AgentCreated(agent_id)
    Gateway-->>User: 201 Created
```

### 4.2 Interactive Session Flow

```mermaid
sequenceDiagram
    participant User
    participant Gateway
    participant Control
    participant Store
    participant Agent
    
    User->>Gateway: POST /v1/agents/{id}/sessions (JWT)
    Gateway->>Control: CreateSession(user_id, agent_id)
    Control->>Store: Verify agent ownership
    Control->>Store: Insert session record
    Control->>Control: Resolve agent pod endpoint
    Control-->>Gateway: Session(session_id, internal_endpoint)
    Gateway-->>User: 201 Created (session_id)
    
    User->>Gateway: WS /v1/sessions/{id}/ws
    Gateway->>Agent: Proxy WebSocket connection
    
    loop Chat
        User->>Gateway: Send message
        Gateway->>Agent: Forward message
        Agent-->>Gateway: Stream response tokens
        Gateway-->>User: Stream response tokens
    end
    
    User->>Gateway: Close WebSocket
    Gateway->>Control: CloseSession(session_id)
    Control->>Store: Update session status
```

---

## 5. Agent Lifecycle

### 5.1 State Machine

```mermaid
stateDiagram-v2
    [*] --> Provisioning: CreateAgent
    
    Provisioning --> Running: Pod ready + health check passes
    Provisioning --> Error: Pod fails to start
    
    Running --> Idle: No active sessions (timeout)
    Running --> Hibernating: HibernateAgent
    Running --> Stopping: StopAgent
    Running --> Error: Health check fails
    
    Idle --> Running: New session / StartAgent
    Idle --> Hibernating: HibernateAgent
    Idle --> Stopping: StopAgent
    
    Hibernating --> Running: WakeAgent / New session
    Hibernating --> Stopping: StopAgent
    
    Stopping --> Stopped: Pod terminated
    
    Stopped --> Provisioning: StartAgent
    Stopped --> [*]: DeleteAgent
    
    Error --> Provisioning: RestartAgent
    Error --> Stopped: StopAgent
    Error --> [*]: DeleteAgent
```

### 5.2 State Descriptions

| State | Description | Pod Status |
|-------|-------------|------------|
| **Provisioning** | Pod being created, Aura initializing | Creating/Pending |
| **Running** | Agent active, accepting sessions | Running |
| **Idle** | No active sessions, still running | Running |
| **Hibernating** | State saved, pod terminated, instant wake | Terminated |
| **Stopping** | Graceful shutdown in progress | Terminating |
| **Stopped** | Pod terminated, state preserved | None |
| **Error** | Health check failed or crash | Failed/CrashLoop |

### 5.3 Hibernation

Hibernation allows cost savings by terminating the pod while preserving state:

1. Agent state is persisted to `/state/<user_id>/<agent_id>/`
2. Pod is terminated (no compute cost)
3. On wake: new pod created, state restored from filesystem
4. Wake triggers: explicit API call, new session request

---

## 6. Storage Architecture

### 6.1 Control Plane Storage (RocksDB)

The control plane uses an embedded RocksDB database:

```
aura-swarm-store/
└── db/
    ├── users/           # User records (from Zero-ID sync)
    ├── agents/          # Agent metadata and status
    ├── agents_by_user/  # Index: user_id -> agent_ids
    └── sessions/        # Active session records
```

Key layout supports future sharding by `user_id` prefix.

### 6.2 Agent State Storage (EFS)

Each agent has isolated persistent storage:

```
/state/
└── <user_id>/
    └── <agent_id>/
        ├── db/          # Aura RocksDB (record, agent_meta, inbox)
        ├── workspaces/  # Agent working directories
        └── config/      # Agent configuration
```

- **Isolation**: Agents can only access their own directory
- **Persistence**: State survives pod restarts and hibernation
- **Backup**: EFS supports snapshots for disaster recovery

---

## 7. Networking Model

### 7.1 Network Topology

```mermaid
graph TB
    subgraph internet [Internet]
        Users[Users]
    end
    
    subgraph vpc [VPC]
        subgraph public_subnet [Public Subnet]
            ALB[Application Load Balancer]
        end
        
        subgraph private_subnet [Private Subnet]
            Gateway[Gateway Pods]
            Control[Control Plane Pods]
        end
        
        subgraph isolated_subnet [Isolated Subnet]
            Agents[Agent MicroVM Pods]
        end
        
        subgraph storage_subnet [Storage]
            EFS[EFS Mount Targets]
        end
    end
    
    Users --> ALB
    ALB --> Gateway
    Gateway --> Control
    Gateway -.-> Agents
    Control --> Agents
    Agents --> EFS
```

### 7.2 Network Policies

| Source | Destination | Allowed |
|--------|-------------|---------|
| Internet | Gateway (443) | Yes |
| Gateway | Control Plane | Yes |
| Gateway | Agent (8080) | Yes (WebSocket proxy) |
| Control Plane | Agent (8080) | Yes (health, lifecycle) |
| Agent | Agent | **No** (cross-agent blocked) |
| Agent | Internet | Allowlist only (LLM APIs) |
| Agent | EFS | Yes (own directory only) |

---

## 8. External Dependencies

### 8.1 Zero-ID

Authentication provider for user identity:

- **Integration**: Simple email/password flow
- **Token**: JWT with `user_id`, `email` claims
- **Validation**: Gateway validates signature and expiry

### 8.2 Aura Runtime

AI agent execution environment:

- **Source**: `github.com/cypher-asi/aura-runtime`
- **Interface**: HTTP health endpoint, WebSocket chat
- **Storage**: RocksDB-based append-only record

### 8.3 Kubernetes + Kata Containers

Execution platform:

- **RuntimeClass**: `kata-fc` (Firecracker backend)
- **Node Requirements**: Kata runtime installed, nested virt or bare metal
- **Pod Spec**: CPU/memory limits, EFS volume mounts

---

## 9. Scalability Considerations

### 9.1 v0.1.0 Limits

| Resource | Limit |
|----------|-------|
| Users | Hundreds |
| Agents per user | 10 |
| Total agents | Thousands |
| Concurrent sessions | Hundreds |

### 9.2 Future Scaling (post-v0.1.0)

- **Cell-based architecture**: Partition users across clusters
- **RocksDB sharding**: Shard by `user_id` prefix
- **Multi-region**: Deploy cells in multiple regions
- **Horizontal gateway**: Stateless gateway pods behind ALB

---

## 10. Crate Dependencies

```mermaid
graph TD
    CORE[aura-swarm-core]
    STORE[aura-swarm-store]
    AUTH[aura-swarm-auth]
    CONTROL[aura-swarm-control]
    SCHEDULER[aura-swarm-scheduler]
    GATEWAY[aura-swarm-gateway]
    
    STORE --> CORE
    AUTH --> CORE
    CONTROL --> CORE
    CONTROL --> STORE
    CONTROL --> AUTH
    SCHEDULER --> CORE
    SCHEDULER --> STORE
    GATEWAY --> CORE
    GATEWAY --> CONTROL
    GATEWAY --> AUTH
```

### 10.1 External Crate Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.x | Async runtime |
| `axum` | 0.7.x | HTTP framework |
| `rocksdb` | 0.22.x | Embedded database |
| `serde` | 1.x | Serialization |
| `jsonwebtoken` | 9.x | JWT validation |
| `kube` | 0.88.x | Kubernetes client |
| `tracing` | 0.1.x | Structured logging |
| `thiserror` | 1.x | Error types |
| `uuid` | 1.x | ID generation |
| `blake3` | 1.x | Hashing |
