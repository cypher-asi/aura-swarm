# Agent Registry — Specification v0.2.0

## 1. Overview

The `aura-swarm-store` crate provides embedded RocksDB storage for the control plane. It stores agent metadata, user references, session records, process-trigger metadata, usage events, and pod-log snapshots — **metadata only**: agent content, secret values, and process payloads never reach this database (they are sealed inside the TEE; see [10-security.md](./10-security.md)).

### 1.1 Responsibilities

- Persist agent records (tier, sealed-storage config, lifecycle state)
- Maintain user/status indexes
- Track sessions
- Store content-free process-trigger metadata for the cron service
- Store price-stamped usage events and capped pod-log termination snapshots
- Run the startup schema migration (v1 → v2; see §6)

### 1.2 Design Principles

- **Single DB**: One RocksDB instance, sharding-ready key layout
- **Column families**: Logical separation of data types; CFs are auto-created on open (`create_missing_column_families`), so adding a CF needs no migration
- **Atomic batches**: State transitions via `WriteBatch`
- **Tagged enums**: CBOR enums (e.g. usage event kinds) dispatch on stored tag strings, never variant order — unknown future variants must not break decode

---

## 2. Storage Schema

### 2.1 Column Families

| Column Family | Purpose | Key Format |
|---------------|---------|------------|
| `agents` | Primary agent records | `agent_id` |
| `agents_by_status` | Status index | `status \| agent_id` |
| `agents_by_user` | User index | `user_id \| agent_id` |
| `sessions` | Session records | `session_id` |
| `sessions_by_agent` | Agent sessions index | `agent_id \| session_id` |
| `users` | User cache (from zOS) | `user_id` |
| `process_triggers` | Trigger metadata registered by agents — only `(process_id, cron, enabled, next_run_at, last_run_at)`; payloads stay sealed in the TEE | `agent_id \| process_id` |
| `usage_events` | Cost/usage events, priced at event time | `agent_id \| timestamp_millis` (big-endian for time-ordered scans) |
| `agent_logs` | Pod-stdout snapshots captured at pod termination; capped per agent (oldest pruned on insert) | `agent_id \| captured_at_millis` (big-endian) |
| `meta` | Database metadata: `schema_version` key (big-endian `u32`); absent key = v1 | fixed keys |

### 2.2 Key Layouts

All keys use big-endian encoding for proper byte ordering.

#### Agents CF

```
Key:   [user_id: 32 bytes][agent_id: 32 bytes]
Value: Agent (CBOR encoded)
```

#### Agents By Status CF

```
Key:   [status: 1 byte][user_id: 32 bytes][agent_id: 32 bytes]
Value: () (empty, index only)
```

#### Sessions CF

```
Key:   [session_id: 16 bytes]
Value: Session (CBOR encoded)
```

#### Sessions By Agent CF

```
Key:   [agent_id: 32 bytes][session_id: 16 bytes]
Value: () (empty, index only)
```

#### Users CF

```
Key:   [user_id: 32 bytes]
Value: User (CBOR encoded)
```

---

## 3. Data Structures

### 3.1 Stored Types

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use swarm_core::{AgentId, UserId, SessionId};

/// Agent record stored in RocksDB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent_id: AgentId,
    pub user_id: UserId,
    pub name: String,
    pub status: u8,  // AgentState as byte
    pub spec: AgentSpecRecord,
    pub created_at: i64,  // Unix timestamp ms
    pub updated_at: i64,
    pub last_heartbeat_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpecRecord {
    pub tier: String,                  // "small" | "standard" | "pro" (required since v2)
    pub cpu_millicores: u32,           // derived from tier
    pub memory_mb: u32,                // derived from tier
    pub isolation: IsolationLevel,     // ConfidentialVM (Container = dev only)
    pub storage_encryption: StorageEncryption, // sealed, key id swarm/agents/{id}/state-key
    pub runtime_version: String,
}

/// Session record stored in RocksDB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub user_id: UserId,
    pub status: u8,  // SessionStatus as byte
    pub created_at: i64,
    pub closed_at: Option<i64>,
}

/// Cached user info from zOS
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub user_id: UserId,
    pub email: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}
```

### 3.2 Key Encoding

```rust
use swarm_core::{AgentId, UserId, SessionId};

pub mod keys {
    /// Primary agent key: agent_id
    pub fn agent_key(agent_id: &AgentId) -> [u8; 32] {
        *agent_id.as_bytes()
    }

    /// User index key: user_id || agent_id (prefix-scan by user)
    pub fn agent_by_user_key(user_id: &UserId, agent_id: &AgentId) -> [u8; 64] {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(user_id.as_bytes());
        key[32..].copy_from_slice(agent_id.as_bytes());
        key
    }

    /// Status index key: status || agent_id
    pub fn status_key(status: u8, agent_id: &AgentId) -> [u8; 33] {
        let mut key = [0u8; 33];
        key[0] = status;
        key[1..].copy_from_slice(agent_id.as_bytes());
        key
    }

    /// Trigger key: agent_id || process_id (utf-8)
    /// Usage-event / log-snapshot keys: agent_id || millis (big-endian u64)
    
    /// Encode session key
    pub fn session_key(session_id: &SessionId) -> [u8; 16] {
        *session_id.as_bytes()
    }
    
    /// Encode session-by-agent key: agent_id || session_id
    pub fn session_by_agent_key(agent_id: &AgentId, session_id: &SessionId) -> [u8; 48] {
        let mut key = [0u8; 48];
        key[..32].copy_from_slice(agent_id.as_bytes());
        key[32..].copy_from_slice(session_id.as_bytes());
        key
    }
    
    /// Encode user key
    pub fn user_key(user_id: &UserId) -> [u8; 32] {
        *user_id.as_bytes()
    }
}
```

---

## 4. Store Trait

### 4.1 Primary Interface

```rust
use swarm_core::{AgentId, UserId, SessionId};

pub trait Store: Send + Sync {
    // Agents
    fn put_agent(&self, agent: &Agent) -> Result<(), StoreError>;
    fn get_agent(&self, agent_id: &AgentId) -> Result<Option<Agent>, StoreError>;
    fn delete_agent(&self, agent_id: &AgentId) -> Result<(), StoreError>;
    fn list_agents_by_user(&self, user_id: &UserId) -> Result<Vec<Agent>, StoreError>;
    fn count_agents_by_user(&self, user_id: &UserId) -> Result<u32, StoreError>;
    fn update_agent_status(&self, agent_id: &AgentId, status: AgentState) -> Result<(), StoreError>;
    fn list_agents_by_status(&self, status: AgentState) -> Result<Vec<Agent>, StoreError>;
    
    // Sessions
    fn put_session(&self, session: &Session) -> Result<(), StoreError>;
    fn get_session(&self, session_id: &SessionId) -> Result<Option<Session>, StoreError>;
    fn update_session_status(&self, session_id: &SessionId, status: SessionStatus) -> Result<(), StoreError>;
    fn list_sessions_by_agent(&self, agent_id: &AgentId) -> Result<Vec<Session>, StoreError>;
    
    // Users
    fn put_user(&self, user: &User) -> Result<(), StoreError>;
    fn get_user(&self, user_id: &UserId) -> Result<Option<User>, StoreError>;
    
    // Admin
    fn list_all_agents(&self) -> Result<Vec<Agent>, StoreError>;
}
```

### 4.2 Error Types

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("RocksDB error: {0}")]
    Rocks(#[from] rocksdb::Error),
    
    #[error("Serialization error: {0}")]
    Serialization(String),
    
    #[error("Key not found")]
    NotFound,
    
    #[error("Invalid data: {0}")]
    InvalidData(String),
}
```

---

## 5. Implementation

### 5.1 RocksDB Store

`RocksStore::open` opens the database with `create_if_missing` + `create_missing_column_families` over the full CF list in `schema.rs` (`agents`, `agents_by_status`, `agents_by_user`, `sessions`, `sessions_by_agent`, `users`, `process_triggers`, `usage_events`, `agent_logs`, `meta`). New CFs are auto-created on open, so adding one requires no migration.

### 5.2 Operation Notes

- **Agents**: primary records keyed by `agent_id`; `put_agent` maintains the `agents_by_user` and `agents_by_status` indexes atomically in one `WriteBatch` (removing stale status-index entries on transition). User-scoped listing prefix-scans `agents_by_user`.
- **Sessions**: primary records keyed by `session_id` with an `agent_id || session_id` index for per-agent listing.
- **Process triggers**: `replace_triggers` swaps an agent's full trigger set atomically (delete-prefix + put batch) — matching the harness's replace-sync registration semantics; single-trigger delete is also supported.
- **Usage events**: append-only, keyed `agent_id || timestamp_millis` (big-endian) so range scans are time-ordered; readers iterate a `[from, to)` key range.
- **Log snapshots**: keyed `agent_id || captured_at_millis`; capped per agent (`LOG_SNAPSHOTS_PER_AGENT_CAP`) with oldest-first pruning on insert.

---

## 6. Schema Versioning and Migration

The `meta` CF holds a `schema_version` key (big-endian `u32`). A database without the key is **v1** (pre-TEE-upgrade). Current version: **v2**.

`migrations::run` executes at gateway startup, before serving traffic:

- **v1 → v2**: rewrites every legacy agent record to the all-TEE shape — resources map to the nearest tier (cpu ≤ 500m → `small`, ≤ 1000m → `standard`, else `pro`), `isolation = confidential_vm`, `storage_encryption = sealed` (key id `swarm/agents/{id}/state-key`). User/status indexes key on fields that don't change, so only primary records are rewritten.
- Records are rewritten in batches of 128. **Idempotency (not batch atomicity) is the crash-safety mechanism**: the version bump lands only in the final batch, so an interrupted migration simply re-runs.
- The legacy (pre-tier, optional-field) CBOR record shape is decoded **only inside `migrations.rs`** — the single surviving legacy decode path, retained for databases restored from pre-R2 backups.
- After migration the control plane backfills missing DEKs in the KBS (see [03-control-plane.md](./03-control-plane.md) §4.3). `/internal/health` reports the current `schema_version`.

---

## 7. Serialization

### 6.1 CBOR Helpers

```rust
use serde::{de::DeserializeOwned, Serialize};

fn cbor_serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .map_err(|e| StoreError::Serialization(e.to_string()))?;
    Ok(buf)
}

fn cbor_deserialize<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    ciborium::from_reader(bytes)
        .map_err(|e| StoreError::Serialization(e.to_string()))
}
```

---

## 8. Future Sharding

### 8.1 Sharding Strategy

The key layout supports sharding by `user_id`:

```
Shard = hash(user_id) % num_shards
```

Each shard is a separate RocksDB instance:

```
/data/
├── shard-00/
│   └── db/
├── shard-01/
│   └── db/
└── shard-02/
    └── db/
```

### 8.2 Sharded Store Interface

```rust
pub struct ShardedStore {
    shards: Vec<RocksStore>,
    num_shards: usize,
}

impl ShardedStore {
    fn shard_for_user(&self, user_id: &UserId) -> &RocksStore {
        let hash = blake3::hash(user_id.as_bytes());
        let shard_idx = (hash.as_bytes()[0] as usize) % self.num_shards;
        &self.shards[shard_idx]
    }
}
```

This is **not implemented in v0.2.0** but the key layout is ready.

---

## 9. Configuration

```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StoreConfig {
    /// Path to RocksDB data directory
    pub data_dir: String,
    
    /// Enable WAL sync on write
    pub sync_writes: bool,
    
    /// Maximum open files
    pub max_open_files: i32,
    
    /// Block cache size in bytes
    pub block_cache_bytes: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            data_dir: "./data/aura-swarm-store".to_string(),
            sync_writes: true,
            max_open_files: 1000,
            block_cache_bytes: 128 * 1024 * 1024, // 128 MB
        }
    }
}
```

---

## 10. Dependencies

### 10.1 Internal

| Crate | Purpose |
|-------|---------|
| `aura-swarm-core` | ID types, domain types |

### 10.2 External

| Crate | Version | Purpose |
|-------|---------|---------|
| `rocksdb` | 0.22.x | Embedded database |
| `ciborium` | 0.2.x | CBOR serialization |
| `serde` | 1.x | Serialization framework |
| `chrono` | 0.4.x | Timestamps |
| `blake3` | 1.x | Hashing for sharding |
| `thiserror` | 1.x | Error types |
