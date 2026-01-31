# Agent Runtime — Specification v0.1.0

## 1. Overview

This document specifies the contract between the MicroVM Agent Platform and the Aura runtime. Each agent runs as an Aura instance inside a Firecracker microVM, with full control over its isolated environment.

### 1.1 Aura Runtime

Aura is a deterministic AI agent runtime that:

- Processes user transactions through a reasoning loop
- Records all actions and effects in an append-only log
- Executes tools (filesystem, commands) within its sandbox
- Maintains persistent state across restarts

### 1.2 Integration Points

```mermaid
graph LR
    Gateway[aura-swarm-gateway] -->|WebSocket| Agent[Aura Runtime]
    Control[aura-swarm-control] -->|HTTP| Agent
    Agent -->|Heartbeat| Control
    Agent -->|R/W| State[/state filesystem]
    
    style Agent fill:#e1f5fe
```

---

## 2. Launch Contract

### 2.1 Environment Variables

The platform launches Aura with these environment variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `AGENT_ID` | Unique agent identifier (64 hex chars) | `a1b2c3d4...` |
| `USER_ID` | Owner's user ID (64 hex chars) | `u1s2e3r4...` |
| `STATE_DIR` | Root directory for persistent state | `/state` |
| `AURA_LISTEN_ADDR` | HTTP/WebSocket listen address | `0.0.0.0:8080` |
| `CONTROL_PLANE_URL` | Control plane heartbeat endpoint | `http://aura-swarm-control:8080` |

### 2.2 Filesystem Layout

```
/state/
├── db/                    # Aura RocksDB (record, agent_meta, inbox)
│   ├── CURRENT
│   ├── MANIFEST-*
│   ├── *.sst
│   └── *.log
├── workspaces/            # Agent working directories
│   └── default/
│       └── ... (user files)
├── config/                # Agent configuration
│   └── agent.toml
└── store/                 # Additional persistent storage
    └── ...
```

### 2.3 Resource Limits

| Resource | Default | Maximum |
|----------|---------|---------|
| CPU | 500m | 4000m |
| Memory | 512Mi | 8Gi |
| State Storage | 10Gi | 100Gi |

---

## 3. Health Contract

### 3.1 Health Endpoint

Aura must expose:

```
GET /health
```

Response when healthy:
```json
{
  "status": "healthy",
  "agent_id": "a1b2c3d4...",
  "uptime_seconds": 3600,
  "version": "0.1.0"
}
```

Response when unhealthy:
```json
{
  "status": "unhealthy",
  "error": "Database connection failed"
}
```

### 3.2 Kubernetes Probes

The platform configures:

- **Readiness Probe**: `GET /health` every 10s, initial delay 5s
- **Liveness Probe**: `GET /health` every 30s, initial delay 30s

Agent is considered:
- **Ready**: When `/health` returns 200 with `status: "healthy"`
- **Failed**: After 3 consecutive failed liveness probes

---

## 4. Interaction Contract

### 4.1 WebSocket Chat Endpoint

Aura must expose:

```
WS /chat
```

This is the primary interface for user interaction.

### 4.2 Message Protocol

#### Client → Agent

**User Message**
```json
{
  "type": "user_message",
  "message_id": "m12345",
  "content": "Read the file src/main.rs"
}
```

**Cancel Request**
```json
{
  "type": "cancel",
  "message_id": "m12345"
}
```

#### Agent → Client

**Assistant Message Start**
```json
{
  "type": "assistant_message_start",
  "message_id": "m67890"
}
```

**Text Delta (streaming)**
```json
{
  "type": "assistant_message_delta",
  "message_id": "m67890",
  "delta": "I'll read that file for you. "
}
```

**Tool Use Start**
```json
{
  "type": "tool_use_start",
  "message_id": "m67890",
  "tool_use_id": "t001",
  "tool_name": "fs.read",
  "input": {
    "path": "src/main.rs"
  }
}
```

**Tool Result**
```json
{
  "type": "tool_result",
  "message_id": "m67890",
  "tool_use_id": "t001",
  "output": "fn main() {\n    println!(\"Hello\");\n}",
  "is_error": false
}
```

**Terminal Output**
```json
{
  "type": "terminal_output",
  "message_id": "m67890",
  "process_id": "p001",
  "stream": "stdout",
  "content": "Compiling project...\n"
}
```

**Assistant Message End**
```json
{
  "type": "assistant_message_end",
  "message_id": "m67890",
  "usage": {
    "input_tokens": 150,
    "output_tokens": 200
  }
}
```

**Error**
```json
{
  "type": "error",
  "message_id": "m67890",
  "code": "tool_execution_failed",
  "message": "Permission denied: /etc/passwd"
}
```

---

## 5. Heartbeat Contract

### 5.1 Heartbeat Endpoint (Agent → Control Plane)

Aura should periodically POST to the control plane:

```
POST {CONTROL_PLANE_URL}/internal/heartbeat
Content-Type: application/json

{
  "agent_id": "a1b2c3d4...",
  "status": "running",
  "uptime_seconds": 3600,
  "active_sessions": 1,
  "record_head_seq": 1234,
  "last_error": null
}
```

### 5.2 Heartbeat Interval

- **Normal**: Every 30 seconds
- **Busy** (active sessions): Every 10 seconds

### 5.3 Heartbeat Response

```json
{
  "ack": true,
  "commands": []
}
```

Future commands may include:
- `{"type": "hibernate"}` — Request graceful hibernation
- `{"type": "shutdown"}` — Request graceful shutdown

---

## 6. Hibernation Contract

### 6.1 Hibernate Endpoint

Control plane calls to initiate hibernation:

```
POST /hibernate
```

Aura must:
1. Complete any in-flight tool executions
2. Close all WebSocket connections gracefully
3. Flush all state to `/state/db/`
4. Respond with success
5. Exit cleanly

Response:
```json
{
  "status": "hibernating",
  "state_saved": true
}
```

### 6.2 Wake Behavior

On restart after hibernation:

1. Aura reads state from `/state/db/`
2. Resumes from last recorded `head_seq`
3. Becomes ready for new sessions

State is fully preserved:
- Conversation history (in RocksDB record)
- Agent memory and beliefs
- Workspace files

---

## 7. Sandbox Environment

### 7.1 Filesystem Access

Aura has **full control** within its `/state` directory:

| Path | Access | Purpose |
|------|--------|---------|
| `/state/` | Read/Write | All agent state |
| `/state/workspaces/` | Read/Write | User files, project directories |
| `/state/db/` | Read/Write | Aura RocksDB |
| `/tmp/` | Read/Write | Temporary files |
| `/` (other) | Read-only | System files |

### 7.2 Tool Capabilities

Aura's tool system has full access within the sandbox:

| Tool | Description | Scope |
|------|-------------|-------|
| `fs.read` | Read file contents | `/state/**` |
| `fs.write` | Write file contents | `/state/**` |
| `fs.ls` | List directory | `/state/**` |
| `fs.edit` | Edit file in place | `/state/**` |
| `cmd.run` | Execute shell command | Sandboxed |
| `search.code` | Search with ripgrep | `/state/**` |

### 7.3 Command Execution

Shell commands run with:
- Working directory: `/state/workspaces/default/`
- User: `aura` (uid 1000)
- No network access (except allowlisted endpoints)
- Resource limits (CPU, memory, time)

### 7.4 Network Access

Outbound network is restricted to:

| Destination | Port | Purpose |
|-------------|------|---------|
| `api.anthropic.com` | 443 | Claude API |
| `api.openai.com` | 443 | OpenAI API |
| Control plane | 8080 | Heartbeat |

All other outbound connections are blocked.

---

## 8. Aura Internal Architecture

Reference: `aura-os` crate implementations

### 8.1 Crate Structure

```
aura/
├─ aura-core          # IDs, schemas, hashing
├─ aura-store         # RocksDB storage
├─ aura-kernel        # Turn processor, policy
├─ aura-swarm         # Runtime orchestration
├─ aura-reasoner      # LLM provider integration
├─ aura-tools         # Tool executors
└─ aura-cli           # CLI interface (optional)
```

### 8.2 Key Types (from aura-core)

```rust
/// Agent identifier - 32 bytes
pub struct AgentId(pub [u8; 32]);

/// Transaction identifier - 32 bytes
pub struct TxId(pub [u8; 32]);

/// Transaction input to the agent
pub struct Transaction {
    pub tx_id: TxId,
    pub agent_id: AgentId,
    pub ts_ms: u64,
    pub kind: TransactionType,
    pub payload: Vec<u8>,
}

/// Record entry (one per processed transaction)
pub struct RecordEntry {
    pub seq: u64,
    pub tx: Transaction,
    pub context_hash: Hash,
    pub proposals: ProposalSet,
    pub decision: Decision,
    pub actions: Vec<Action>,
    pub effects: Vec<Effect>,
}
```

### 8.3 Storage (from aura-store)

```rust
/// Column families
pub const CF_RECORD: &str = "record";       // R|agent_id|seq -> RecordEntry
pub const CF_AGENT_META: &str = "agent_meta"; // M|agent_id|field -> value
pub const CF_INBOX: &str = "inbox";         // Q|agent_id|seq -> Transaction

/// Store trait
pub trait Store: Send + Sync {
    fn enqueue_tx(&self, tx: &Transaction) -> Result<()>;
    fn dequeue_tx(&self, agent_id: AgentId) -> Result<Option<(u64, Transaction)>>;
    fn get_head_seq(&self, agent_id: AgentId) -> Result<u64>;
    fn append_entry_atomic(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        dequeued_inbox_seq: u64,
    ) -> Result<()>;
}
```

---

## 9. HTTP Endpoints Summary

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/health` | GET | Health check (required) |
| `/chat` | WS | Interactive session (required) |
| `/hibernate` | POST | Graceful hibernation (required) |
| `/status` | GET | Detailed status (optional) |
| `/metrics` | GET | Prometheus metrics (optional) |

---

## 10. Error Handling

### 10.1 Error Codes

| Code | Description |
|------|-------------|
| `agent_not_ready` | Agent still initializing |
| `tool_execution_failed` | Tool returned error |
| `tool_not_found` | Unknown tool name |
| `tool_timeout` | Tool execution timed out |
| `model_error` | LLM API error |
| `rate_limited` | Too many requests |
| `internal_error` | Unexpected error |

### 10.2 Recovery Behavior

On error during processing:
1. Error is recorded in `RecordEntry.effects`
2. Agent remains operational
3. User receives error message via WebSocket
4. Retry is possible with new message

On fatal error:
1. Agent logs error and exits
2. Kubernetes restarts pod
3. State restored from `/state/db/`

---

## 11. Configuration

### 11.1 Agent Configuration File

`/state/config/agent.toml`:

```toml
[agent]
name = "my-agent"

[model]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
max_tokens = 4096

[tools]
enabled = ["fs.read", "fs.write", "fs.ls", "fs.edit", "search.code", "cmd.run"]
command_allowlist = ["ls", "cat", "grep", "find", "cargo", "npm", "python"]

[limits]
max_tool_calls_per_turn = 10
tool_timeout_seconds = 60
max_file_read_bytes = 10485760  # 10MB

[workspace]
default_dir = "/state/workspaces/default"
```

### 11.2 Environment Overrides

Environment variables override config file:

| Variable | Config Path |
|----------|-------------|
| `AURA_MODEL_PROVIDER` | `model.provider` |
| `AURA_MODEL_NAME` | `model.model` |
| `ANTHROPIC_API_KEY` | (secret) |
| `OPENAI_API_KEY` | (secret) |

---

## 12. Metrics

### 12.1 Exposed Metrics

If `/metrics` endpoint is implemented:

```prometheus
# Agent uptime
aura_uptime_seconds{agent_id="..."} 3600

# Record sequence
aura_record_head_seq{agent_id="..."} 1234

# Active sessions
aura_active_sessions{agent_id="..."} 1

# Tool executions
aura_tool_executions_total{agent_id="...", tool="fs.read", status="success"} 50
aura_tool_execution_duration_seconds{agent_id="...", tool="fs.read"} 0.05

# Model calls
aura_model_calls_total{agent_id="...", model="claude-sonnet-4", status="success"} 100
aura_model_tokens_input_total{agent_id="..."} 15000
aura_model_tokens_output_total{agent_id="..."} 8000
```
