//! `RocksDB` storage implementation.
//!
//! This module provides the `RocksStore` implementation of the `Store` trait.

use std::path::Path;
use std::sync::Arc;

use aura_swarm_core::{AgentId, SessionId, UserId};
use rocksdb::{
    BoundColumnFamily, ColumnFamilyDescriptor, DBWithThreadMode, IteratorMode, MultiThreaded,
    Options, WriteBatch,
};

use crate::error::{Result, StoreError};
use crate::keys;
use crate::schema::{all_column_families, cf};
use crate::types::{Agent, AgentState, ProcessTrigger, Session, SessionStatus, User};
use crate::Store;

/// RocksDB-backed storage implementation.
pub struct RocksStore {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
}

impl RocksStore {
    /// Open or create a `RocksDB` database at the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or created.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let cf_descriptors: Vec<_> = all_column_families()
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default()))
            .collect();

        let db = DBWithThreadMode::open_cf_descriptors(&opts, path, cf_descriptors)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Get a column family handle.
    fn cf(&self, name: &str) -> Result<Arc<BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| StoreError::Database(format!("column family not found: {name}")))
    }

    /// Serialize a value using CBOR.
    fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        ciborium::into_writer(value, &mut buf)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        Ok(buf)
    }

    /// Deserialize a value from CBOR.
    fn deserialize<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T> {
        ciborium::from_reader(data).map_err(|e| StoreError::Serialization(e.to_string()))
    }
}

impl Store for RocksStore {
    // =========================================================================
    // Agent Operations
    // =========================================================================

    fn put_agent(&self, agent: &Agent) -> Result<()> {
        let cf_agents = self.cf(cf::AGENTS)?;
        let cf_by_user = self.cf(cf::AGENTS_BY_USER)?;
        let cf_by_status = self.cf(cf::AGENTS_BY_STATUS)?;

        let agent_key = keys::agent_key(&agent.agent_id);
        let user_agent_key = keys::user_agent_key(&agent.user_id, &agent.agent_id);
        let status_agent_key = keys::status_agent_key(agent.status.as_u8(), &agent.agent_id);
        let value = Self::serialize(agent)?;

        // Check if agent exists to handle status index updates
        let old_status = self
            .db
            .get_cf(&cf_agents, &agent_key)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .map(|data| Self::deserialize::<Agent>(&data))
            .transpose()?
            .map(|a| a.status);

        let mut batch = WriteBatch::default();

        // Update main record
        batch.put_cf(&cf_agents, &agent_key, &value);

        // Update user index (idempotent)
        batch.put_cf(&cf_by_user, &user_agent_key, []);

        // Update status index if status changed
        if let Some(old) = old_status {
            if old != agent.status {
                // Remove old status index
                let old_status_key = keys::status_agent_key(old.as_u8(), &agent.agent_id);
                batch.delete_cf(&cf_by_status, &old_status_key);
            }
        }
        batch.put_cf(&cf_by_status, &status_agent_key, []);

        self.db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    fn get_agent(&self, agent_id: &AgentId) -> Result<Option<Agent>> {
        let cf = self.cf(cf::AGENTS)?;
        let key = keys::agent_key(agent_id);

        self.db
            .get_cf(&cf, key)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .map(|data| Self::deserialize(&data))
            .transpose()
    }

    fn delete_agent(&self, agent_id: &AgentId) -> Result<()> {
        let cf_agents = self.cf(cf::AGENTS)?;
        let cf_by_user = self.cf(cf::AGENTS_BY_USER)?;
        let cf_by_status = self.cf(cf::AGENTS_BY_STATUS)?;

        // Get the agent to find user_id and status
        let agent = self.get_agent(agent_id)?.ok_or(StoreError::NotFound)?;

        let agent_key = keys::agent_key(agent_id);
        let user_agent_key = keys::user_agent_key(&agent.user_id, agent_id);
        let status_agent_key = keys::status_agent_key(agent.status.as_u8(), agent_id);

        let mut batch = WriteBatch::default();
        batch.delete_cf(&cf_agents, &agent_key);
        batch.delete_cf(&cf_by_user, &user_agent_key);
        batch.delete_cf(&cf_by_status, &status_agent_key);

        self.db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    fn list_agents_by_user(&self, user_id: &UserId) -> Result<Vec<Agent>> {
        let cf_by_user = self.cf(cf::AGENTS_BY_USER)?;
        let prefix = keys::user_prefix(user_id);

        let mut agents = Vec::new();
        let iter = self.db.iterator_cf(
            &cf_by_user,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (key, _) = item.map_err(|e| StoreError::Database(e.to_string()))?;

            // Stop if we're past the prefix
            if !key.starts_with(&prefix) {
                break;
            }

            let agent_id = keys::extract_agent_id_from_user_agent_key(&key);
            if let Some(agent) = self.get_agent(&agent_id)? {
                agents.push(agent);
            }
        }

        Ok(agents)
    }

    fn count_agents_by_user(&self, user_id: &UserId) -> Result<u32> {
        let cf_by_user = self.cf(cf::AGENTS_BY_USER)?;
        let prefix = keys::user_prefix(user_id);

        let mut count = 0u32;
        let iter = self.db.iterator_cf(
            &cf_by_user,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (key, _) = item.map_err(|e| StoreError::Database(e.to_string()))?;

            if !key.starts_with(&prefix) {
                break;
            }

            count += 1;
        }

        Ok(count)
    }

    fn list_agents_by_status(&self, status: AgentState) -> Result<Vec<Agent>> {
        let cf_by_status = self.cf(cf::AGENTS_BY_STATUS)?;
        let prefix = keys::status_prefix(status.as_u8());

        let mut agents = Vec::new();
        let iter = self.db.iterator_cf(
            &cf_by_status,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (key, _) = item.map_err(|e| StoreError::Database(e.to_string()))?;

            if !key.starts_with(&prefix) {
                break;
            }

            // Extract agent_id from key (skip the status byte)
            let mut agent_bytes = [0u8; 16];
            agent_bytes.copy_from_slice(&key[1..17]);
            let agent_id = AgentId::from_bytes(agent_bytes);

            if let Some(agent) = self.get_agent(&agent_id)? {
                agents.push(agent);
            }
        }

        Ok(agents)
    }

    fn update_agent_status(&self, agent_id: &AgentId, status: AgentState) -> Result<()> {
        let mut agent = self.get_agent(agent_id)?.ok_or(StoreError::NotFound)?;
        agent.status = status;
        agent.updated_at = chrono::Utc::now();
        // Clear error message when not in error state
        if status != AgentState::Error {
            agent.error_message = None;
        }
        self.put_agent(&agent)
    }

    fn update_agent_error(
        &self,
        agent_id: &AgentId,
        status: AgentState,
        error_message: Option<String>,
    ) -> Result<()> {
        let mut agent = self.get_agent(agent_id)?.ok_or(StoreError::NotFound)?;
        agent.status = status;
        agent.error_message = error_message;
        agent.updated_at = chrono::Utc::now();
        self.put_agent(&agent)
    }

    fn list_all_agents(&self) -> Result<Vec<Agent>> {
        let cf = self.cf(cf::AGENTS)?;

        let mut agents = Vec::new();
        let iter = self.db.iterator_cf(&cf, IteratorMode::Start);

        for item in iter {
            let (_, value) = item.map_err(|e| StoreError::Database(e.to_string()))?;
            let agent: Agent = Self::deserialize(&value)?;
            agents.push(agent);
        }

        Ok(agents)
    }

    // =========================================================================
    // Session Operations
    // =========================================================================

    fn put_session(&self, session: &Session) -> Result<()> {
        let cf_sessions = self.cf(cf::SESSIONS)?;
        let cf_by_agent = self.cf(cf::SESSIONS_BY_AGENT)?;

        let session_key = keys::session_key(&session.session_id);
        let agent_session_key = keys::agent_session_key(&session.agent_id, &session.session_id);
        let value = Self::serialize(session)?;

        let mut batch = WriteBatch::default();
        batch.put_cf(&cf_sessions, &session_key, &value);
        batch.put_cf(&cf_by_agent, &agent_session_key, []);

        self.db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    fn get_session(&self, session_id: &SessionId) -> Result<Option<Session>> {
        let cf = self.cf(cf::SESSIONS)?;
        let key = keys::session_key(session_id);

        self.db
            .get_cf(&cf, key)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .map(|data| Self::deserialize(&data))
            .transpose()
    }

    fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        let cf_sessions = self.cf(cf::SESSIONS)?;
        let cf_by_agent = self.cf(cf::SESSIONS_BY_AGENT)?;

        // Get the session to find agent_id
        let session = self.get_session(session_id)?.ok_or(StoreError::NotFound)?;

        let session_key = keys::session_key(session_id);
        let agent_session_key = keys::agent_session_key(&session.agent_id, session_id);

        let mut batch = WriteBatch::default();
        batch.delete_cf(&cf_sessions, &session_key);
        batch.delete_cf(&cf_by_agent, &agent_session_key);

        self.db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    fn list_sessions_by_agent(&self, agent_id: &AgentId) -> Result<Vec<Session>> {
        let cf_by_agent = self.cf(cf::SESSIONS_BY_AGENT)?;
        let prefix = keys::agent_prefix(agent_id);

        let mut sessions = Vec::new();
        let iter = self.db.iterator_cf(
            &cf_by_agent,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (key, _) = item.map_err(|e| StoreError::Database(e.to_string()))?;

            if !key.starts_with(&prefix) {
                break;
            }

            let session_id = keys::extract_session_id_from_agent_session_key(&key);
            if let Some(session) = self.get_session(&session_id)? {
                sessions.push(session);
            }
        }

        Ok(sessions)
    }

    fn update_session_status(&self, session_id: &SessionId, status: SessionStatus) -> Result<()> {
        let mut session = self.get_session(session_id)?.ok_or(StoreError::NotFound)?;
        session.status = status;
        if status == SessionStatus::Closed {
            session.closed_at = Some(chrono::Utc::now());
        }
        self.put_session(&session)
    }

    // =========================================================================
    // User Operations
    // =========================================================================

    fn put_user(&self, user: &User) -> Result<()> {
        let cf = self.cf(cf::USERS)?;
        let key = keys::user_key(&user.user_id);
        let value = Self::serialize(user)?;

        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    fn get_user(&self, user_id: &UserId) -> Result<Option<User>> {
        let cf = self.cf(cf::USERS)?;
        let key = keys::user_key(user_id);

        self.db
            .get_cf(&cf, key)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .map(|data| Self::deserialize(&data))
            .transpose()
    }

    // =========================================================================
    // Process Trigger Operations (Swarm TEE upgrade phase 8)
    // =========================================================================

    fn put_process_trigger(&self, trigger: &ProcessTrigger) -> Result<()> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;
        let key = keys::process_trigger_key(&trigger.agent_id, &trigger.process_id);
        let value = Self::serialize(trigger)?;

        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    fn get_process_trigger(
        &self,
        agent_id: &AgentId,
        process_id: &str,
    ) -> Result<Option<ProcessTrigger>> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;
        let key = keys::process_trigger_key(agent_id, process_id);

        self.db
            .get_cf(&cf, key)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .map(|data| Self::deserialize(&data))
            .transpose()
    }

    fn list_process_triggers_by_agent(&self, agent_id: &AgentId) -> Result<Vec<ProcessTrigger>> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;
        let prefix = keys::agent_prefix(agent_id);

        let mut triggers = Vec::new();
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (key, value) = item.map_err(|e| StoreError::Database(e.to_string()))?;
            if !key.starts_with(&prefix) {
                break;
            }
            triggers.push(Self::deserialize(&value)?);
        }

        Ok(triggers)
    }

    fn list_all_process_triggers(&self) -> Result<Vec<ProcessTrigger>> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;

        let mut triggers = Vec::new();
        for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
            let (_, value) = item.map_err(|e| StoreError::Database(e.to_string()))?;
            triggers.push(Self::deserialize(&value)?);
        }

        Ok(triggers)
    }

    fn delete_process_trigger(&self, agent_id: &AgentId, process_id: &str) -> Result<()> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;
        let key = keys::process_trigger_key(agent_id, process_id);

        if self
            .db
            .get_cf(&cf, &key)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .is_none()
        {
            return Err(StoreError::NotFound);
        }

        self.db
            .delete_cf(&cf, key)
            .map_err(|e| StoreError::Database(e.to_string()))
    }

    fn delete_process_triggers_for_agent(&self, agent_id: &AgentId) -> Result<u32> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;
        let prefix = keys::agent_prefix(agent_id);

        let mut batch = WriteBatch::default();
        let mut count = 0u32;
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );
        for item in iter {
            let (key, _) = item.map_err(|e| StoreError::Database(e.to_string()))?;
            if !key.starts_with(&prefix) {
                break;
            }
            batch.delete_cf(&cf, key);
            count += 1;
        }

        self.db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(count)
    }

    fn replace_process_triggers(
        &self,
        agent_id: &AgentId,
        triggers: Vec<ProcessTrigger>,
    ) -> Result<Vec<ProcessTrigger>> {
        let cf = self.cf(cf::PROCESS_TRIGGERS)?;

        // Existing set, for bookkeeping preservation + removal of
        // triggers absent from the new desired set.
        let existing = self.list_process_triggers_by_agent(agent_id)?;

        let mut batch = WriteBatch::default();
        let mut stored = Vec::with_capacity(triggers.len());

        for old in &existing {
            if !triggers.iter().any(|t| t.process_id == old.process_id) {
                batch.delete_cf(&cf, keys::process_trigger_key(agent_id, &old.process_id));
            }
        }

        for mut trigger in triggers {
            trigger.agent_id = *agent_id;
            if let Some(old) = existing.iter().find(|o| o.process_id == trigger.process_id) {
                // Re-registration of a known trigger: keep gateway-side
                // bookkeeping, take the agent-supplied schedule fields.
                trigger.registered_at = old.registered_at;
                trigger.last_run_at = old.last_run_at;
            }
            let key = keys::process_trigger_key(agent_id, &trigger.process_id);
            batch.put_cf(&cf, key, Self::serialize(&trigger)?);
            stored.push(trigger);
        }

        self.db
            .write(batch)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentSpec, SessionConfig};
    use tempfile::TempDir;

    fn create_test_store() -> (RocksStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = RocksStore::open(dir.path()).unwrap();
        (store, dir)
    }

    fn create_test_agent(user_id: &UserId, name: &str) -> Agent {
        Agent {
            agent_id: AgentId::generate_deterministic(user_id, name, 42),
            user_id: *user_id,
            name: name.to_string(),
            status: AgentState::Running,
            spec: AgentSpec::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_heartbeat_at: None,
            error_message: None,
        }
    }

    #[test]
    fn agent_crud() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent = create_test_agent(&user_id, "test-agent");

        // Create
        store.put_agent(&agent).unwrap();

        // Read
        let retrieved = store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(retrieved.name, agent.name);
        assert_eq!(retrieved.status, AgentState::Running);

        // Update
        store
            .update_agent_status(&agent.agent_id, AgentState::Idle)
            .unwrap();
        let updated = store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(updated.status, AgentState::Idle);

        // Delete
        store.delete_agent(&agent.agent_id).unwrap();
        assert!(store.get_agent(&agent.agent_id).unwrap().is_none());
    }

    #[test]
    fn list_agents_by_user() {
        let (store, _dir) = create_test_store();
        let user1 = UserId::from_uuid(uuid::Uuid::new_v4());
        let user2 = UserId::from_uuid(uuid::Uuid::new_v4());

        // Create agents for user1
        let agent1a = create_test_agent(&user1, "agent-1a");
        let agent1b = create_test_agent(&user1, "agent-1b");
        store.put_agent(&agent1a).unwrap();
        store.put_agent(&agent1b).unwrap();

        // Create agent for user2
        let agent2 = create_test_agent(&user2, "agent-2");
        store.put_agent(&agent2).unwrap();

        // List user1's agents
        let user1_agents = store.list_agents_by_user(&user1).unwrap();
        assert_eq!(user1_agents.len(), 2);

        // List user2's agents
        let user2_agents = store.list_agents_by_user(&user2).unwrap();
        assert_eq!(user2_agents.len(), 1);

        // Count
        assert_eq!(store.count_agents_by_user(&user1).unwrap(), 2);
        assert_eq!(store.count_agents_by_user(&user2).unwrap(), 1);
    }

    #[test]
    fn list_agents_by_status() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());

        let mut agent1 = create_test_agent(&user_id, "agent-1");
        agent1.status = AgentState::Running;
        store.put_agent(&agent1).unwrap();

        let mut agent2 = create_test_agent(&user_id, "agent-2");
        agent2.status = AgentState::Idle;
        store.put_agent(&agent2).unwrap();

        let mut agent3 = create_test_agent(&user_id, "agent-3");
        agent3.status = AgentState::Running;
        store.put_agent(&agent3).unwrap();

        let running = store.list_agents_by_status(AgentState::Running).unwrap();
        assert_eq!(running.len(), 2);

        let idle = store.list_agents_by_status(AgentState::Idle).unwrap();
        assert_eq!(idle.len(), 1);
    }

    #[test]
    fn status_index_updated_on_change() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent = create_test_agent(&user_id, "agent");

        store.put_agent(&agent).unwrap();
        assert_eq!(
            store
                .list_agents_by_status(AgentState::Running)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store.list_agents_by_status(AgentState::Idle).unwrap().len(),
            0
        );

        // Update status
        store
            .update_agent_status(&agent.agent_id, AgentState::Idle)
            .unwrap();
        assert_eq!(
            store
                .list_agents_by_status(AgentState::Running)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            store.list_agents_by_status(AgentState::Idle).unwrap().len(),
            1
        );
    }

    #[test]
    fn session_crud() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent = create_test_agent(&user_id, "agent");
        store.put_agent(&agent).unwrap();

        let session = Session {
            session_id: SessionId::generate(),
            agent_id: agent.agent_id,
            user_id,
            status: SessionStatus::Active,
            config: SessionConfig::default(),
            run_id: None,
            created_at: chrono::Utc::now(),
            closed_at: None,
        };

        // Create
        store.put_session(&session).unwrap();

        // Read
        let retrieved = store.get_session(&session.session_id).unwrap().unwrap();
        assert_eq!(retrieved.status, SessionStatus::Active);

        // Update
        store
            .update_session_status(&session.session_id, SessionStatus::Closed)
            .unwrap();
        let updated = store.get_session(&session.session_id).unwrap().unwrap();
        assert_eq!(updated.status, SessionStatus::Closed);
        assert!(updated.closed_at.is_some());

        // Delete
        store.delete_session(&session.session_id).unwrap();
        assert!(store.get_session(&session.session_id).unwrap().is_none());
    }

    #[test]
    fn list_sessions_by_agent() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());

        let agent1 = create_test_agent(&user_id, "agent-1");
        let agent2 = create_test_agent(&user_id, "agent-2");
        store.put_agent(&agent1).unwrap();
        store.put_agent(&agent2).unwrap();

        // Create sessions for agent1
        for _ in 0..3 {
            let session = Session {
                session_id: SessionId::generate(),
                agent_id: agent1.agent_id,
                user_id,
                status: SessionStatus::Active,
                config: SessionConfig::default(),
                run_id: None,
                created_at: chrono::Utc::now(),
                closed_at: None,
            };
            store.put_session(&session).unwrap();
        }

        // Create session for agent2
        let session2 = Session {
            session_id: SessionId::generate(),
            agent_id: agent2.agent_id,
            user_id,
            status: SessionStatus::Active,
            config: SessionConfig::default(),
            run_id: None,
            created_at: chrono::Utc::now(),
            closed_at: None,
        };
        store.put_session(&session2).unwrap();

        // List sessions by agent
        let agent1_sessions = store.list_sessions_by_agent(&agent1.agent_id).unwrap();
        assert_eq!(agent1_sessions.len(), 3);

        let agent2_sessions = store.list_sessions_by_agent(&agent2.agent_id).unwrap();
        assert_eq!(agent2_sessions.len(), 1);
    }

    #[test]
    fn user_crud() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());

        let user = User {
            user_id,
            email: "test@example.com".to_string(),
            email_verified: true,
            created_at: chrono::Utc::now(),
            last_login_at: None,
        };

        // Create
        store.put_user(&user).unwrap();

        // Read
        let retrieved = store.get_user(&user_id).unwrap().unwrap();
        assert_eq!(retrieved.email, "test@example.com");

        // Non-existent user
        let other_id = UserId::from_uuid(uuid::Uuid::new_v4());
        assert!(store.get_user(&other_id).unwrap().is_none());
    }

    #[test]
    fn delete_agent_not_found() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "ghost", 1);

        let result = store.delete_agent(&agent_id);
        assert!(matches!(result, Err(StoreError::NotFound)));
    }

    #[test]
    fn update_agent_status_not_found() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "ghost", 2);

        let result = store.update_agent_status(&agent_id, AgentState::Idle);
        assert!(matches!(result, Err(StoreError::NotFound)));
    }

    #[test]
    fn delete_session_not_found() {
        let (store, _dir) = create_test_store();
        let session_id = SessionId::generate();

        let result = store.delete_session(&session_id);
        assert!(matches!(result, Err(StoreError::NotFound)));
    }

    #[test]
    fn list_all_agents_multiple_users() {
        let (store, _dir) = create_test_store();
        let user1 = UserId::from_uuid(uuid::Uuid::new_v4());
        let user2 = UserId::from_uuid(uuid::Uuid::new_v4());

        let a1 = create_test_agent(&user1, "agent-u1");
        let a2 = create_test_agent(&user2, "agent-u2");
        store.put_agent(&a1).unwrap();
        store.put_agent(&a2).unwrap();

        let all = store.list_all_agents().unwrap();
        assert_eq!(all.len(), 2);
    }

    fn create_test_trigger(agent_id: &AgentId, process_id: &str) -> ProcessTrigger {
        let now = chrono::Utc::now();
        ProcessTrigger {
            agent_id: *agent_id,
            process_id: process_id.to_string(),
            cron: "0 * * * *".to_string(),
            enabled: true,
            next_run_at: Some(now + chrono::Duration::hours(1)),
            last_run_at: None,
            registered_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn process_trigger_crud() {
        let (store, _dir) = create_test_store();
        let agent_id = AgentId::generate();
        let trigger = create_test_trigger(&agent_id, "proc-1");

        // Upsert + get
        store.put_process_trigger(&trigger).unwrap();
        let got = store
            .get_process_trigger(&agent_id, "proc-1")
            .unwrap()
            .unwrap();
        assert_eq!(got, trigger);

        // Upsert overwrites
        let mut updated = trigger.clone();
        updated.enabled = false;
        store.put_process_trigger(&updated).unwrap();
        let got = store
            .get_process_trigger(&agent_id, "proc-1")
            .unwrap()
            .unwrap();
        assert!(!got.enabled);

        // Delete
        store.delete_process_trigger(&agent_id, "proc-1").unwrap();
        assert!(store
            .get_process_trigger(&agent_id, "proc-1")
            .unwrap()
            .is_none());

        // Delete of unknown trigger is NotFound
        let result = store.delete_process_trigger(&agent_id, "proc-1");
        assert!(matches!(result, Err(StoreError::NotFound)));
    }

    #[test]
    fn process_triggers_list_by_agent_and_all() {
        let (store, _dir) = create_test_store();
        let agent1 = AgentId::generate();
        let agent2 = AgentId::generate();

        store
            .put_process_trigger(&create_test_trigger(&agent1, "a"))
            .unwrap();
        store
            .put_process_trigger(&create_test_trigger(&agent1, "b"))
            .unwrap();
        store
            .put_process_trigger(&create_test_trigger(&agent2, "c"))
            .unwrap();

        assert_eq!(store.list_process_triggers_by_agent(&agent1).unwrap().len(), 2);
        assert_eq!(store.list_process_triggers_by_agent(&agent2).unwrap().len(), 1);
        assert_eq!(store.list_all_process_triggers().unwrap().len(), 3);
    }

    #[test]
    fn delete_process_triggers_for_agent_scoped() {
        let (store, _dir) = create_test_store();
        let agent1 = AgentId::generate();
        let agent2 = AgentId::generate();

        store
            .put_process_trigger(&create_test_trigger(&agent1, "a"))
            .unwrap();
        store
            .put_process_trigger(&create_test_trigger(&agent1, "b"))
            .unwrap();
        store
            .put_process_trigger(&create_test_trigger(&agent2, "c"))
            .unwrap();

        let removed = store.delete_process_triggers_for_agent(&agent1).unwrap();
        assert_eq!(removed, 2);
        assert!(store.list_process_triggers_by_agent(&agent1).unwrap().is_empty());
        // Other agent untouched
        assert_eq!(store.list_process_triggers_by_agent(&agent2).unwrap().len(), 1);

        // No-op for an agent with no triggers
        assert_eq!(store.delete_process_triggers_for_agent(&agent1).unwrap(), 0);
    }

    #[test]
    fn replace_process_triggers_semantics() {
        let (store, _dir) = create_test_store();
        let agent_id = AgentId::generate();

        // Seed: triggers "a" (with cron-service bookkeeping) and "b".
        let mut a = create_test_trigger(&agent_id, "a");
        let original_registered_at = a.registered_at - chrono::Duration::days(1);
        a.registered_at = original_registered_at;
        a.last_run_at = Some(a.registered_at + chrono::Duration::hours(1));
        store.put_process_trigger(&a).unwrap();
        store
            .put_process_trigger(&create_test_trigger(&agent_id, "b"))
            .unwrap();

        // Replace with desired set { a (new cron, disabled), c }:
        // "b" must vanish, "a" must keep registered_at / last_run_at.
        let mut new_a = create_test_trigger(&agent_id, "a");
        new_a.cron = "*/5 * * * *".to_string();
        new_a.enabled = false;
        let new_c = create_test_trigger(&agent_id, "c");

        let stored = store
            .replace_process_triggers(&agent_id, vec![new_a, new_c])
            .unwrap();
        assert_eq!(stored.len(), 2);

        let listed = store.list_process_triggers_by_agent(&agent_id).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(store.get_process_trigger(&agent_id, "b").unwrap().is_none());

        let got_a = store.get_process_trigger(&agent_id, "a").unwrap().unwrap();
        assert_eq!(got_a.cron, "*/5 * * * *");
        assert!(!got_a.enabled);
        assert_eq!(got_a.registered_at, original_registered_at);
        assert_eq!(got_a.last_run_at, a.last_run_at);

        assert!(store.get_process_trigger(&agent_id, "c").unwrap().is_some());

        // Replace with the empty set clears everything.
        let stored = store.replace_process_triggers(&agent_id, vec![]).unwrap();
        assert!(stored.is_empty());
        assert!(store.list_process_triggers_by_agent(&agent_id).unwrap().is_empty());
    }

    #[test]
    fn update_agent_error_with_message() {
        let (store, _dir) = create_test_store();
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent = create_test_agent(&user_id, "err-agent");
        store.put_agent(&agent).unwrap();

        store
            .update_agent_error(
                &agent.agent_id,
                AgentState::Error,
                Some("pod crash".to_string()),
            )
            .unwrap();

        let updated = store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(updated.status, AgentState::Error);
        assert_eq!(updated.error_message, Some("pod crash".to_string()));
    }
}
