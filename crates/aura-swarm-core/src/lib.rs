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
//! // Parse a user ID from hex
//! let user_id = UserId::from_hex(
//!     "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
//! ).unwrap();
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
pub use ids::{AgentId, IdError, IdentityId, NamespaceId, SessionId, UserId};
