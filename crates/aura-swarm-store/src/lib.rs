//! `RocksDB` storage layer for aura-swarm.
//!
//! This crate provides persistent storage for agents, sessions, and users using `RocksDB`
//! with column families for efficient indexing.
//!
//! # Architecture
//!
//! The storage uses the following column families:
//!
//! - `agents`: Primary agent records, keyed by `agent_id`
//! - `agents_by_status`: Index for listing agents by status
//! - `agents_by_user`: Index for listing agents by user
//! - `sessions`: Primary session records, keyed by `session_id`
//! - `sessions_by_agent`: Index for listing sessions by agent
//! - `users`: User records synced from zOS
//! - `process_triggers`: Trigger metadata registered by agents
//! - `usage_events`: Usage/cost events keyed by agent and timestamp
//!
//! # Example
//!
//! ```no_run
//! use aura_swarm_store::{RocksStore, Store};
//! use aura_swarm_core::UserId;
//!
//! let store = RocksStore::open("/tmp/aura-swarm-db").unwrap();
//!
//! // List agents for a user
//! let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
//! let agents = store.list_agents_by_user(&user_id).unwrap();
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod error;
pub mod keys;
pub mod rocks;
pub mod schema;
pub mod types;

pub use error::{Result, StoreError};
pub use rocks::RocksStore;
pub use types::{
    Agent, AgentSpec, AgentState, BoxTier, IsolationLevel, ProcessTrigger, Session, SessionConfig,
    SessionStatus, StorageEncryption, UnknownBoxTier, User, WorkspaceConfig,
};

use aura_swarm_core::{AgentId, SessionId, UserId};

/// The storage trait defining all database operations.
///
/// This trait abstracts the storage layer, allowing for different implementations
/// (e.g., `RocksDB`, in-memory for testing).
pub trait Store: Send + Sync {
    // =========================================================================
    // Agent Operations
    // =========================================================================

    /// Insert or update an agent record.
    ///
    /// This also maintains the user and status indexes.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn put_agent(&self, agent: &Agent) -> Result<()>;

    /// Get an agent by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn get_agent(&self, agent_id: &AgentId) -> Result<Option<Agent>>;

    /// Delete an agent by ID.
    ///
    /// This also removes the agent from all indexes.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` if the agent doesn't exist.
    fn delete_agent(&self, agent_id: &AgentId) -> Result<()>;

    /// List all agents belonging to a user.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn list_agents_by_user(&self, user_id: &UserId) -> Result<Vec<Agent>>;

    /// Count agents belonging to a user.
    ///
    /// This is more efficient than listing when you only need the count.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn count_agents_by_user(&self, user_id: &UserId) -> Result<u32>;

    /// List all agents with a given status.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn list_agents_by_status(&self, status: AgentState) -> Result<Vec<Agent>>;

    /// Update an agent's status.
    ///
    /// This is a convenience method that also updates the status index atomically.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` if the agent doesn't exist.
    fn update_agent_status(&self, agent_id: &AgentId, status: AgentState) -> Result<()>;

    /// Update an agent's status with an error message.
    ///
    /// Use this when transitioning to an Error state to provide context.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` if the agent doesn't exist.
    fn update_agent_error(
        &self,
        agent_id: &AgentId,
        status: AgentState,
        error_message: Option<String>,
    ) -> Result<()>;

    /// List all agents in the database.
    ///
    /// Use with caution in production; prefer filtered queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn list_all_agents(&self) -> Result<Vec<Agent>>;

    // =========================================================================
    // Session Operations
    // =========================================================================

    /// Insert or update a session record.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn put_session(&self, session: &Session) -> Result<()>;

    /// Get a session by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn get_session(&self, session_id: &SessionId) -> Result<Option<Session>>;

    /// Delete a session by ID.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` if the session doesn't exist.
    fn delete_session(&self, session_id: &SessionId) -> Result<()>;

    /// List all sessions for an agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn list_sessions_by_agent(&self, agent_id: &AgentId) -> Result<Vec<Session>>;

    /// Update a session's status.
    ///
    /// If setting to `Closed`, also sets `closed_at`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` if the session doesn't exist.
    fn update_session_status(&self, session_id: &SessionId, status: SessionStatus) -> Result<()>;

    // =========================================================================
    // User Operations
    // =========================================================================

    /// Insert or update a user record.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn put_user(&self, user: &User) -> Result<()>;

    /// Get a user by user ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn get_user(&self, user_id: &UserId) -> Result<Option<User>>;

    // =========================================================================
    // Process Trigger Operations (Swarm TEE upgrade phase 8)
    // =========================================================================
    //
    // Trust boundary: a `ProcessTrigger` carries only the trigger
    // metadata an agent exported (`process_id`, `cron`, `enabled`,
    // `next_run_at`) plus control-plane bookkeeping. Process payloads
    // never reach this store.

    /// Insert or update a process trigger (keyed by
    /// `agent_id || process_id`).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn put_process_trigger(&self, trigger: &ProcessTrigger) -> Result<()>;

    /// Get a process trigger by agent and process id.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn get_process_trigger(
        &self,
        agent_id: &AgentId,
        process_id: &str,
    ) -> Result<Option<ProcessTrigger>>;

    /// List all triggers registered for an agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn list_process_triggers_by_agent(&self, agent_id: &AgentId) -> Result<Vec<ProcessTrigger>>;

    /// List every registered trigger across all agents.
    ///
    /// Used by the control-plane cron service to scan for due triggers.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn list_all_process_triggers(&self) -> Result<Vec<ProcessTrigger>>;

    /// Delete a single process trigger.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` if no such trigger is registered.
    fn delete_process_trigger(&self, agent_id: &AgentId, process_id: &str) -> Result<()>;

    /// Delete every trigger registered for an agent (agent destroy:
    /// triggers must not outlive the agent). Returns the number of
    /// triggers removed; deleting for an agent with none is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn delete_process_triggers_for_agent(&self, agent_id: &AgentId) -> Result<u32>;

    /// Atomically replace the full trigger set for an agent with the
    /// given desired set (replace-semantics sync from the agent VM).
    ///
    /// Triggers absent from `triggers` are removed; for triggers that
    /// already exist, the control-plane bookkeeping (`registered_at`,
    /// `last_run_at`) is preserved while `cron` / `enabled` /
    /// `next_run_at` / `updated_at` are taken from the new record.
    /// Returns the stored set.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn replace_process_triggers(
        &self,
        agent_id: &AgentId,
        triggers: Vec<ProcessTrigger>,
    ) -> Result<Vec<ProcessTrigger>>;
}
