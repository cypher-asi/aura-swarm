# Agent Registry — Specification v0.1.0

## 1. Overview

The `aura-swarm-store` crate provides embedded RocksDB storage for the control plane. It stores agent metadata, user references, and session records with key layouts designed for efficient queries and future sharding.

### 1.1 Responsibilities

- Persist agent records with lifecycle state
- Maintain user-to-agents index
- Track active sessions
- Provide atomic operations for state transitions
- Support efficient prefix scans for user-scoped queries

### 1.2 Design Principles

- **Single DB for v0.1.0**: One RocksDB instance, sharding-ready key layout
- **User-prefixed keys**: All agent data keyed by `user_id` for future partitioning
- **Column families**: Logical separation of data types
- **Atomic batches**: State transitions via `WriteBatch`

---

## 2. Storage Schema

### 2.1 Column Families

| Column Family | Purpose | Key Format |
|---------------|---------|------------|
| `agents` | Agent records | `user_id \| agent_id` |
| `agents_by_status` | Status index | `status \| user_id \| agent_id` |
| `sessions` | Session records | `session_id` |
| `sessions_by_agent` | Agent sessions index | `agent_id \| session_id` |
| `users` | User cache (from Zero-ID) | `user_id` |

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
    pub cpu_millicores: u32,
    pub memory_mb: u32,
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

/// Cached user info from Zero-ID
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
    /// Encode agent key: user_id || agent_id
    pub fn agent_key(user_id: &UserId, agent_id: &AgentId) -> [u8; 64] {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(user_id.as_bytes());
        key[32..].copy_from_slice(agent_id.as_bytes());
        key
    }
    
    /// Encode agent prefix for scanning all agents of a user
    pub fn agent_prefix(user_id: &UserId) -> [u8; 32] {
        *user_id.as_bytes()
    }
    
    /// Encode status index key: status || user_id || agent_id
    pub fn status_key(status: u8, user_id: &UserId, agent_id: &AgentId) -> [u8; 65] {
        let mut key = [0u8; 65];
        key[0] = status;
        key[1..33].copy_from_slice(user_id.as_bytes());
        key[33..].copy_from_slice(agent_id.as_bytes());
        key
    }
    
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

```rust
use rocksdb::{DB, Options, ColumnFamilyDescriptor, WriteBatch};
use std::path::Path;
use std::sync::Arc;

pub struct RocksStore {
    db: Arc<DB>,
}

impl RocksStore {
    /// Column family names
    const CF_AGENTS: &'static str = "agents";
    const CF_AGENTS_BY_STATUS: &'static str = "agents_by_status";
    const CF_SESSIONS: &'static str = "sessions";
    const CF_SESSIONS_BY_AGENT: &'static str = "sessions_by_agent";
    const CF_USERS: &'static str = "users";
    
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        
        let cfs = vec![
            ColumnFamilyDescriptor::new(Self::CF_AGENTS, Options::default()),
            ColumnFamilyDescriptor::new(Self::CF_AGENTS_BY_STATUS, Options::default()),
            ColumnFamilyDescriptor::new(Self::CF_SESSIONS, Options::default()),
            ColumnFamilyDescriptor::new(Self::CF_SESSIONS_BY_AGENT, Options::default()),
            ColumnFamilyDescriptor::new(Self::CF_USERS, Options::default()),
        ];
        
        let db = DB::open_cf_descriptors(&opts, path, cfs)?;
        
        Ok(Self { db: Arc::new(db) })
    }
}
```

### 5.2 Agent Operations

```rust
impl Store for RocksStore {
    fn put_agent(&self, agent: &Agent) -> Result<(), StoreError> {
        let cf_agents = self.db.cf_handle(Self::CF_AGENTS).unwrap();
        let cf_status = self.db.cf_handle(Self::CF_AGENTS_BY_STATUS).unwrap();
        
        let key = keys::agent_key(&agent.user_id, &agent.agent_id);
        let value = cbor_serialize(agent)?;
        let status_key = keys::status_key(agent.status as u8, &agent.user_id, &agent.agent_id);
        
        // Check if agent exists (to update status index)
        let old_status = if let Some(old_value) = self.db.get_cf(&cf_agents, &key)? {
            let old_agent: AgentRecord = cbor_deserialize(&old_value)?;
            Some(old_agent.status)
        } else {
            None
        };
        
        let mut batch = WriteBatch::default();
        
        // Put agent record
        batch.put_cf(&cf_agents, &key, &value);
        
        // Update status index
        if let Some(old) = old_status {
            if old != agent.status as u8 {
                // Remove old status index
                let old_status_key = keys::status_key(old, &agent.user_id, &agent.agent_id);
                batch.delete_cf(&cf_status, &old_status_key);
            }
        }
        batch.put_cf(&cf_status, &status_key, &[]);
        
        self.db.write(batch)?;
        Ok(())
    }
    
    fn get_agent(&self, agent_id: &AgentId) -> Result<Option<Agent>, StoreError> {
        let cf = self.db.cf_handle(Self::CF_AGENTS).unwrap();
        
        // We need to scan by agent_id since we don't have user_id
        // This is O(n) but acceptable for v0.1.0
        let iter = self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start);
        
        for item in iter {
            let (key, value) = item?;
            if key.len() == 64 && &key[32..] == agent_id.as_bytes() {
                let record: AgentRecord = cbor_deserialize(&value)?;
                return Ok(Some(record.into()));
            }
        }
        
        Ok(None)
    }
    
    fn list_agents_by_user(&self, user_id: &UserId) -> Result<Vec<Agent>, StoreError> {
        let cf = self.db.cf_handle(Self::CF_AGENTS).unwrap();
        let prefix = keys::agent_prefix(user_id);
        
        let iter = self.db.prefix_iterator_cf(&cf, &prefix);
        let mut agents = Vec::new();
        
        for item in iter {
            let (key, value) = item?;
            // Verify prefix match (prefix_iterator may overshoot)
            if key.starts_with(&prefix) {
                let record: AgentRecord = cbor_deserialize(&value)?;
                agents.push(record.into());
            } else {
                break;
            }
        }
        
        Ok(agents)
    }
    
    fn count_agents_by_user(&self, user_id: &UserId) -> Result<u32, StoreError> {
        let agents = self.list_agents_by_user(user_id)?;
        Ok(agents.len() as u32)
    }
    
    fn update_agent_status(&self, agent_id: &AgentId, status: AgentState) -> Result<(), StoreError> {
        if let Some(mut agent) = self.get_agent(agent_id)? {
            agent.status = status;
            agent.updated_at = chrono::Utc::now();
            self.put_agent(&agent)?;
        }
        Ok(())
    }
    
    fn delete_agent(&self, agent_id: &AgentId) -> Result<(), StoreError> {
        let cf_agents = self.db.cf_handle(Self::CF_AGENTS).unwrap();
        let cf_status = self.db.cf_handle(Self::CF_AGENTS_BY_STATUS).unwrap();
        
        // Find the agent to get user_id and status
        if let Some(agent) = self.get_agent(agent_id)? {
            let key = keys::agent_key(&agent.user_id, agent_id);
            let status_key = keys::status_key(agent.status as u8, &agent.user_id, agent_id);
            
            let mut batch = WriteBatch::default();
            batch.delete_cf(&cf_agents, &key);
            batch.delete_cf(&cf_status, &status_key);
            self.db.write(batch)?;
        }
        
        Ok(())
    }
}
```

### 5.3 Session Operations

```rust
impl RocksStore {
    fn put_session(&self, session: &Session) -> Result<(), StoreError> {
        let cf_sessions = self.db.cf_handle(Self::CF_SESSIONS).unwrap();
        let cf_by_agent = self.db.cf_handle(Self::CF_SESSIONS_BY_AGENT).unwrap();
        
        let key = keys::session_key(&session.session_id);
        let index_key = keys::session_by_agent_key(&session.agent_id, &session.session_id);
        let value = cbor_serialize(session)?;
        
        let mut batch = WriteBatch::default();
        batch.put_cf(&cf_sessions, &key, &value);
        batch.put_cf(&cf_by_agent, &index_key, &[]);
        self.db.write(batch)?;
        
        Ok(())
    }
    
    fn get_session(&self, session_id: &SessionId) -> Result<Option<Session>, StoreError> {
        let cf = self.db.cf_handle(Self::CF_SESSIONS).unwrap();
        let key = keys::session_key(session_id);
        
        if let Some(value) = self.db.get_cf(&cf, &key)? {
            let record: SessionRecord = cbor_deserialize(&value)?;
            return Ok(Some(record.into()));
        }
        
        Ok(None)
    }
    
    fn list_sessions_by_agent(&self, agent_id: &AgentId) -> Result<Vec<Session>, StoreError> {
        let cf_by_agent = self.db.cf_handle(Self::CF_SESSIONS_BY_AGENT).unwrap();
        let cf_sessions = self.db.cf_handle(Self::CF_SESSIONS).unwrap();
        
        let prefix = agent_id.as_bytes();
        let iter = self.db.prefix_iterator_cf(&cf_by_agent, prefix);
        
        let mut sessions = Vec::new();
        
        for item in iter {
            let (key, _) = item?;
            if !key.starts_with(prefix) {
                break;
            }
            
            // Extract session_id from key
            if key.len() == 48 {
                let mut session_id_bytes = [0u8; 16];
                session_id_bytes.copy_from_slice(&key[32..]);
                let session_id = SessionId::new(session_id_bytes);
                
                if let Some(session) = self.get_session(&session_id)? {
                    sessions.push(session);
                }
            }
        }
        
        Ok(sessions)
    }
}
```

---

## 6. Serialization

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

## 7. Future Sharding

### 7.1 Sharding Strategy

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

### 7.2 Sharded Store Interface

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

This is **not implemented in v0.1.0** but the key layout is ready.

---

## 8. Configuration

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

## 9. Dependencies

### 9.1 Internal

| Crate | Purpose |
|-------|---------|
| `aura-swarm-core` | ID types, domain types |

### 9.2 External

| Crate | Version | Purpose |
|-------|---------|---------|
| `rocksdb` | 0.22.x | Embedded database |
| `ciborium` | 0.2.x | CBOR serialization |
| `serde` | 1.x | Serialization framework |
| `chrono` | 0.4.x | Timestamps |
| `blake3` | 1.x | Hashing for sharding |
| `thiserror` | 1.x | Error types |
