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
//! - `users`: User records synced from Zero-ID
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
//! let user_id = UserId::from_bytes([0u8; 32]);
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
pub use types::{Agent, AgentSpec, AgentState, IsolationLevel, Session, SessionStatus, User};

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

    /// Get a user by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    fn get_user(&self, user_id: &UserId) -> Result<Option<User>>;
}
