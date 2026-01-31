//! Common error types for aura-swarm.
//!
//! This module provides shared error types that are used across multiple crates.

use crate::ids::{AgentId, SessionId};
use thiserror::Error;

/// A result type using `CoreError`.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Core errors that can occur throughout the aura-swarm system.
#[derive(Debug, Error)]
pub enum CoreError {
    /// An agent with the specified ID was not found.
    #[error("agent not found: {0}")]
    AgentNotFound(AgentId),

    /// A session with the specified ID was not found.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// An invalid identifier was provided.
    #[error("invalid identifier: {0}")]
    InvalidId(#[from] crate::ids::IdError),

    /// An internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}
