//! Agent lifecycle state machine.
//!
//! This module defines the valid state transitions for agents and provides
//! validation logic to ensure state machine invariants are maintained.
//!
//! # State Machine
//!
//! ```text
//!                    ┌─────────────────┐
//!                    │  Provisioning   │
//!                    └────────┬────────┘
//!                             │ (pod ready)
//!                             ▼
//!     ┌──────────────────────────────────────────────┐
//!     │                   Running                     │◄──────┐
//!     └──────────────────────────────────────────────┘       │
//!           │               │                │                │
//!           │ (idle)        │ (hibernate)    │ (stop)        │ (wake)
//!           ▼               ▼                ▼                │
//!     ┌─────────┐    ┌─────────────┐   ┌──────────┐          │
//!     │  Idle   │───▶│ Hibernating │───┘          │          │
//!     └────┬────┘    └─────────────┘              │          │
//!          │                                      ▼          │
//!          │                              ┌───────────┐      │
//!          └─────────────────────────────▶│  Stopping │      │
//!                                         └─────┬─────┘      │
//!                                               │            │
//!                                               ▼            │
//!                                         ┌──────────┐       │
//!                                         │  Stopped │───────┘
//!                                         └──────────┘
//!                                               │
//!                                               ▼
//!                                         ┌──────────┐
//!                                         │  Error   │
//!                                         └──────────┘
//! ```

use aura_swarm_core::AgentId;
use aura_swarm_store::AgentState;

use crate::error::{ControlError, Result};

/// Validates a state transition and returns the target state if valid.
///
/// # Errors
///
/// Returns `ControlError::InvalidState` if the transition is not allowed.
pub fn validate_transition(
    agent_id: &AgentId,
    from: AgentState,
    to: AgentState,
) -> Result<AgentState> {
    if is_valid_transition(from, to) {
        Ok(to)
    } else {
        Err(ControlError::InvalidState {
            agent_id: *agent_id,
            from,
            to,
        })
    }
}

/// Check if a state transition is valid according to the state machine.
#[must_use]
pub const fn is_valid_transition(from: AgentState, to: AgentState) -> bool {
    use AgentState::{Error, Hibernating, Idle, Provisioning, Running, Stopped, Stopping};

    matches!(
        (from, to),
        // Provisioning can only go to Running (on success) or Error
        // Running, Idle can go to Error
        (Provisioning | Idle, Running)
            | (Provisioning | Running | Idle | Hibernating | Stopping, Error)
            // Running can go to Idle, Hibernating, or Stopping
            | (Running, Idle | Hibernating | Stopping)
            // Idle can go to Hibernating or Stopping
            | (Idle, Hibernating | Stopping)
            // Hibernating can go to Running (instant wake) or Provisioning (for scheduler) or Stopping
            | (Hibernating, Running | Provisioning | Stopping)
            // Stopping can go to Stopped; Error can also go to Stopped
            | (Stopping | Error, Stopped)
            // Stopped and Error can go to Provisioning (restart/retry)
            | (Stopped | Error, Provisioning)
    )
}

/// Returns the list of valid target states from the given state.
#[must_use]
pub fn valid_transitions_from(state: AgentState) -> Vec<AgentState> {
    use AgentState::{Error, Hibernating, Idle, Provisioning, Running, Stopped, Stopping};

    match state {
        Provisioning => vec![Running, Error],
        Running => vec![Idle, Hibernating, Stopping, Error],
        Idle => vec![Running, Hibernating, Stopping, Error],
        Hibernating => vec![Running, Provisioning, Stopping, Error],
        Stopping => vec![Stopped, Error],
        Stopped => vec![Provisioning],
        Error => vec![Stopped, Provisioning],
    }
}

/// Returns true if the agent is in a state where it can accept sessions.
#[must_use]
pub const fn can_accept_sessions(state: AgentState) -> bool {
    matches!(state, AgentState::Running | AgentState::Idle)
}

/// Returns true if the agent is in a state where it can be woken.
#[must_use]
pub const fn can_wake(state: AgentState) -> bool {
    matches!(state, AgentState::Hibernating | AgentState::Stopped)
}

/// Returns true if the agent is in a terminal state (stopped or error).
#[must_use]
pub const fn is_terminal(state: AgentState) -> bool {
    matches!(state, AgentState::Stopped | AgentState::Error)
}

/// Returns true if the agent is actively running (has a pod).
#[must_use]
pub const fn is_active(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Provisioning | AgentState::Running | AgentState::Idle | AgentState::Stopping
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        use AgentState::*;

        // Provisioning -> Running (success)
        assert!(is_valid_transition(Provisioning, Running));
        // Running -> Idle (no sessions)
        assert!(is_valid_transition(Running, Idle));
        // Running -> Hibernating (explicit hibernate)
        assert!(is_valid_transition(Running, Hibernating));
        // Idle -> Running (new session)
        assert!(is_valid_transition(Idle, Running));
        // Hibernating -> Running (wake)
        assert!(is_valid_transition(Hibernating, Running));
        // Stopping -> Stopped (shutdown complete)
        assert!(is_valid_transition(Stopping, Stopped));
        // Stopped -> Provisioning (restart)
        assert!(is_valid_transition(Stopped, Provisioning));
    }

    #[test]
    fn invalid_transitions() {
        use AgentState::*;

        // Can't go backwards to Provisioning from Running
        assert!(!is_valid_transition(Running, Provisioning));
        // Can't skip from Provisioning to Stopped
        assert!(!is_valid_transition(Provisioning, Stopped));
        // Can't go from Stopped to Running directly
        assert!(!is_valid_transition(Stopped, Running));
        // Can't go from Hibernating to Idle
        assert!(!is_valid_transition(Hibernating, Idle));
    }

    #[test]
    fn validate_transition_ok() {
        let agent_id = AgentId::from_bytes([1u8; 32]);
        let result = validate_transition(&agent_id, AgentState::Running, AgentState::Idle);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AgentState::Idle);
    }

    #[test]
    fn validate_transition_err() {
        let agent_id = AgentId::from_bytes([1u8; 32]);
        let result = validate_transition(&agent_id, AgentState::Stopped, AgentState::Running);
        assert!(result.is_err());

        match result {
            Err(ControlError::InvalidState { from, to, .. }) => {
                assert_eq!(from, AgentState::Stopped);
                assert_eq!(to, AgentState::Running);
            }
            _ => panic!("expected InvalidState error"),
        }
    }

    #[test]
    fn session_acceptance() {
        assert!(can_accept_sessions(AgentState::Running));
        assert!(can_accept_sessions(AgentState::Idle));
        assert!(!can_accept_sessions(AgentState::Hibernating));
        assert!(!can_accept_sessions(AgentState::Stopped));
    }

    #[test]
    fn wake_eligibility() {
        assert!(can_wake(AgentState::Hibernating));
        assert!(can_wake(AgentState::Stopped));
        assert!(!can_wake(AgentState::Running));
        assert!(!can_wake(AgentState::Idle));
    }

    #[test]
    fn terminal_states() {
        assert!(is_terminal(AgentState::Stopped));
        assert!(is_terminal(AgentState::Error));
        assert!(!is_terminal(AgentState::Running));
        assert!(!is_terminal(AgentState::Hibernating));
    }

    #[test]
    fn active_states() {
        assert!(is_active(AgentState::Running));
        assert!(is_active(AgentState::Idle));
        assert!(is_active(AgentState::Provisioning));
        assert!(is_active(AgentState::Stopping));
        assert!(!is_active(AgentState::Stopped));
        assert!(!is_active(AgentState::Hibernating));
    }

    #[test]
    fn valid_transitions_from_running() {
        let transitions = valid_transitions_from(AgentState::Running);
        assert!(transitions.contains(&AgentState::Idle));
        assert!(transitions.contains(&AgentState::Hibernating));
        assert!(transitions.contains(&AgentState::Stopping));
        assert!(transitions.contains(&AgentState::Error));
        assert!(!transitions.contains(&AgentState::Provisioning));
    }
}
