//! Core types and utilities for aura-swarm.
//!
//! This crate provides the foundational types used throughout the aura-swarm platform:
//!
//! - **Identifiers**: Strongly-typed IDs for users, agents, and sessions
//! - **Error types**: Common error definitions shared across crates
//!
//! # Example
//!
//! ```
//! use aura_swarm_core::{UserId, AgentId, SessionId};
//!
//! // Create a user ID from a UUID
//! let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
//!
//! // Generate an agent ID
//! let agent_id = AgentId::generate(&user_id, "my-agent");
//!
//! // Generate a session ID
//! let session_id = SessionId::generate();
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod error;
pub mod ids;

pub use error::{CoreError, Result};
pub use ids::{AgentId, IdError, SessionId, UserId};
