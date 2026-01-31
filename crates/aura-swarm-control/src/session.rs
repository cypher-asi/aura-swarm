//! Session management operations.
//!
//! This module provides session lifecycle operations including creation,
//! retrieval, and closing of sessions. Sessions are the primary way users
//! interact with their agents.

use aura_swarm_core::{AgentId, SessionId, UserId};
use aura_swarm_store::{Agent, AgentState, Session, SessionStatus, Store};
use chrono::Utc;

use crate::error::{ControlError, Result};
use crate::lifecycle;

/// Create a new session for an agent.
///
/// If the agent is in a wakeable state (Hibernating or Stopped), it will be
/// automatically woken. If the agent is Idle, it will be transitioned to Running.
///
/// # Errors
///
/// Returns an error if:
/// - The agent is not found
/// - The user is not the owner
/// - The agent is not in a state that can accept sessions
pub fn create_session<S: Store>(
    store: &S,
    user_id: &UserId,
    agent_id: &AgentId,
) -> Result<(Session, Option<AgentState>)> {
    let agent = store
        .get_agent(agent_id)?
        .ok_or(ControlError::AgentNotFound(*agent_id))?;

    // Verify ownership
    if agent.user_id != *user_id {
        return Err(ControlError::NotOwner {
            user_id: *user_id,
            agent_id: *agent_id,
        });
    }

    // Determine if we need to wake the agent
    let state_change = determine_state_for_session(&agent)?;

    // If we need to change state, do it
    if let Some(new_state) = state_change {
        store.update_agent_status(agent_id, new_state)?;
    }

    // Create the session
    let session = Session {
        session_id: SessionId::generate(),
        agent_id: *agent_id,
        user_id: *user_id,
        status: SessionStatus::Active,
        created_at: Utc::now(),
        closed_at: None,
    };

    store.put_session(&session)?;

    Ok((session, state_change))
}

/// Determine what state change (if any) is needed for a session to be created.
fn determine_state_for_session(agent: &Agent) -> Result<Option<AgentState>> {
    match agent.status {
        // Running: no change needed
        AgentState::Running => Ok(None),

        // Idle or Hibernating: transition/wake to Running
        AgentState::Idle | AgentState::Hibernating => Ok(Some(AgentState::Running)),

        // Stopped: need to provision again
        AgentState::Stopped => Ok(Some(AgentState::Provisioning)),

        // Provisioning, Stopping, or Error: can't create session
        AgentState::Provisioning | AgentState::Stopping | AgentState::Error => {
            Err(ControlError::AgentNotRunnable(agent.agent_id))
        }
    }
}

/// Get a session by ID, verifying ownership.
///
/// # Errors
///
/// Returns an error if:
/// - The session is not found
/// - The user is not the owner
pub fn get_session<S: Store>(
    store: &S,
    user_id: &UserId,
    session_id: &SessionId,
) -> Result<Session> {
    let session = store
        .get_session(session_id)?
        .ok_or(ControlError::SessionNotFound(*session_id))?;

    if session.user_id != *user_id {
        return Err(ControlError::NotOwner {
            user_id: *user_id,
            agent_id: session.agent_id,
        });
    }

    Ok(session)
}

/// Close a session.
///
/// If this is the last active session for the agent, the agent will transition
/// to Idle state.
///
/// # Errors
///
/// Returns an error if:
/// - The session is not found
/// - The user is not the owner
pub fn close_session<S: Store>(
    store: &S,
    user_id: &UserId,
    session_id: &SessionId,
) -> Result<bool> {
    let session = get_session(store, user_id, session_id)?;

    if session.status == SessionStatus::Closed {
        return Ok(false); // Already closed
    }

    // Close the session
    store.update_session_status(session_id, SessionStatus::Closed)?;

    // Check if this was the last active session
    let active_sessions = count_active_sessions(store, &session.agent_id)?;

    if active_sessions == 0 {
        // Transition agent to Idle if it was Running
        if let Some(agent) = store.get_agent(&session.agent_id)? {
            if agent.status == AgentState::Running
                && lifecycle::is_valid_transition(AgentState::Running, AgentState::Idle)
            {
                store.update_agent_status(&session.agent_id, AgentState::Idle)?;
            }
        }
    }

    Ok(true)
}

/// List all sessions for an agent, verifying ownership.
///
/// # Errors
///
/// Returns an error if:
/// - The agent is not found
/// - The user is not the owner
pub fn list_sessions<S: Store>(
    store: &S,
    user_id: &UserId,
    agent_id: &AgentId,
) -> Result<Vec<Session>> {
    let agent = store
        .get_agent(agent_id)?
        .ok_or(ControlError::AgentNotFound(*agent_id))?;

    if agent.user_id != *user_id {
        return Err(ControlError::NotOwner {
            user_id: *user_id,
            agent_id: *agent_id,
        });
    }

    Ok(store.list_sessions_by_agent(agent_id)?)
}

/// Count active sessions for an agent.
pub(crate) fn count_active_sessions<S: Store>(store: &S, agent_id: &AgentId) -> Result<usize> {
    let sessions = store.list_sessions_by_agent(agent_id)?;
    Ok(sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Active)
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_store::{AgentSpec, RocksStore};
    use tempfile::TempDir;

    fn setup() -> (RocksStore, TempDir, UserId, Agent) {
        let dir = TempDir::new().unwrap();
        let store = RocksStore::open(dir.path()).unwrap();
        let user_id = UserId::from_bytes([1u8; 32]);
        let agent = Agent {
            agent_id: AgentId::generate_deterministic(&user_id, "test-agent", 42),
            user_id,
            name: "test-agent".to_string(),
            status: AgentState::Running,
            spec: AgentSpec::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_heartbeat_at: None,
        };
        store.put_agent(&agent).unwrap();
        (store, dir, user_id, agent)
    }

    #[test]
    fn create_session_running_agent() {
        let (store, _dir, user_id, agent) = setup();

        let (session, state_change) = create_session(&store, &user_id, &agent.agent_id).unwrap();

        assert_eq!(session.agent_id, agent.agent_id);
        assert_eq!(session.user_id, user_id);
        assert_eq!(session.status, SessionStatus::Active);
        assert!(state_change.is_none()); // No state change needed
    }

    #[test]
    fn create_session_idle_agent() {
        let (store, _dir, user_id, mut agent) = setup();

        // Set agent to Idle
        agent.status = AgentState::Idle;
        store.put_agent(&agent).unwrap();

        let (session, state_change) = create_session(&store, &user_id, &agent.agent_id).unwrap();

        assert_eq!(session.status, SessionStatus::Active);
        assert_eq!(state_change, Some(AgentState::Running));

        // Verify agent state was updated
        let updated_agent = store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(updated_agent.status, AgentState::Running);
    }

    #[test]
    fn create_session_hibernating_agent() {
        let (store, _dir, user_id, mut agent) = setup();

        // Set agent to Hibernating
        agent.status = AgentState::Hibernating;
        store.put_agent(&agent).unwrap();

        let (session, state_change) = create_session(&store, &user_id, &agent.agent_id).unwrap();

        assert_eq!(session.status, SessionStatus::Active);
        assert_eq!(state_change, Some(AgentState::Running));
    }

    #[test]
    fn create_session_not_owner() {
        let (store, _dir, _user_id, agent) = setup();
        let other_user = UserId::from_bytes([99u8; 32]);

        let result = create_session(&store, &other_user, &agent.agent_id);

        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }

    #[test]
    fn create_session_not_runnable() {
        let (store, _dir, user_id, mut agent) = setup();

        // Set agent to Error state
        agent.status = AgentState::Error;
        store.put_agent(&agent).unwrap();

        let result = create_session(&store, &user_id, &agent.agent_id);

        assert!(matches!(result, Err(ControlError::AgentNotRunnable(_))));
    }

    #[test]
    fn close_session_transitions_to_idle() {
        let (store, _dir, user_id, agent) = setup();

        // Create a session
        let (session, _) = create_session(&store, &user_id, &agent.agent_id).unwrap();

        // Close it
        let closed = close_session(&store, &user_id, &session.session_id).unwrap();
        assert!(closed);

        // Verify session is closed
        let updated = store.get_session(&session.session_id).unwrap().unwrap();
        assert_eq!(updated.status, SessionStatus::Closed);

        // Verify agent is now Idle
        let updated_agent = store.get_agent(&agent.agent_id).unwrap().unwrap();
        assert_eq!(updated_agent.status, AgentState::Idle);
    }

    #[test]
    fn list_sessions_verifies_ownership() {
        let (store, _dir, _user_id, agent) = setup();
        let other_user = UserId::from_bytes([99u8; 32]);

        let result = list_sessions(&store, &other_user, &agent.agent_id);

        assert!(matches!(result, Err(ControlError::NotOwner { .. })));
    }
}
