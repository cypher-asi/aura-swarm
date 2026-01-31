//! Error types for the control plane.
//!
//! This module defines all errors that can occur during agent lifecycle
//! and session management operations.

use aura_swarm_core::{AgentId, SessionId, UserId};
use aura_swarm_store::AgentState;
use thiserror::Error;

/// A result type using `ControlError`.
pub type Result<T> = std::result::Result<T, ControlError>;

/// Errors that can occur in control plane operations.
#[derive(Debug, Error)]
pub enum ControlError {
    /// The requested agent was not found.
    #[error("agent not found: {0}")]
    AgentNotFound(AgentId),

    /// The requested session was not found.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// The user has reached their agent quota limit.
    #[error("agent quota exceeded for user {user_id}: limit is {limit}")]
    QuotaExceeded {
        /// The user who exceeded the quota.
        user_id: UserId,
        /// The maximum number of agents allowed.
        limit: u32,
    },

    /// The user is not the owner of the requested resource.
    #[error("user {user_id} is not the owner of agent {agent_id}")]
    NotOwner {
        /// The user making the request.
        user_id: UserId,
        /// The agent being accessed.
        agent_id: AgentId,
    },

    /// The requested state transition is not valid.
    #[error(
        "invalid state transition for agent {agent_id}: cannot transition from {from:?} to {to:?}"
    )]
    InvalidState {
        /// The agent being transitioned.
        agent_id: AgentId,
        /// The current state.
        from: AgentState,
        /// The requested target state.
        to: AgentState,
    },

    /// The agent is not in a runnable state.
    #[error("agent {0} is not in a runnable state")]
    AgentNotRunnable(AgentId),

    /// A session is already active for this agent.
    #[error("agent {0} already has an active session")]
    SessionAlreadyActive(AgentId),

    /// Storage layer error.
    #[error("storage error: {0}")]
    Store(#[from] aura_swarm_store::StoreError),

    /// Authentication error.
    #[error("authentication error: {0}")]
    Auth(#[from] aura_swarm_auth::AuthError),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ControlError {
    /// Returns the appropriate HTTP status code for this error.
    #[must_use]
    pub const fn http_status_code(&self) -> u16 {
        match self {
            Self::AgentNotFound(_) | Self::SessionNotFound(_) => 404,
            Self::QuotaExceeded { .. } => 429,
            Self::NotOwner { .. } => 403,
            Self::InvalidState { .. }
            | Self::AgentNotRunnable(_)
            | Self::SessionAlreadyActive(_) => 409,
            Self::Store(_) | Self::Internal(_) => 500,
            Self::Auth(_) => 401,
        }
    }

    /// Returns true if this error might be resolved by retrying.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        matches!(self, Self::Store(_) | Self::Internal(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_status_codes() {
        let agent_id = AgentId::from_bytes([1u8; 32]);
        let session_id = aura_swarm_core::SessionId::generate();
        let user_id = UserId::from_bytes([2u8; 32]);

        assert_eq!(
            ControlError::AgentNotFound(agent_id).http_status_code(),
            404
        );
        assert_eq!(
            ControlError::SessionNotFound(session_id).http_status_code(),
            404
        );
        assert_eq!(
            ControlError::QuotaExceeded { user_id, limit: 10 }.http_status_code(),
            429
        );
        assert_eq!(
            ControlError::NotOwner { user_id, agent_id }.http_status_code(),
            403
        );
        assert_eq!(
            ControlError::InvalidState {
                agent_id,
                from: AgentState::Running,
                to: AgentState::Provisioning
            }
            .http_status_code(),
            409
        );
    }
}
