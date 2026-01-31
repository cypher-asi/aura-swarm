//! Key encoding utilities for `RocksDB`.
//!
//! This module provides functions to encode and decode keys for various indexes.
//! All keys are designed to support efficient prefix scans.

use aura_swarm_core::{AgentId, SessionId, UserId};

/// Encode an agent key (just the agent ID bytes).
#[must_use]
pub fn agent_key(agent_id: &AgentId) -> Vec<u8> {
    agent_id.as_bytes().to_vec()
}

/// Encode a user-agent index key: `user_id || agent_id`.
///
/// This allows efficient prefix scans for all agents belonging to a user.
#[must_use]
pub fn user_agent_key(user_id: &UserId, agent_id: &AgentId) -> Vec<u8> {
    let mut key = Vec::with_capacity(64);
    key.extend_from_slice(user_id.as_bytes());
    key.extend_from_slice(agent_id.as_bytes());
    key
}

/// Encode a user prefix for scanning all agents by user.
#[must_use]
pub fn user_prefix(user_id: &UserId) -> Vec<u8> {
    user_id.as_bytes().to_vec()
}

/// Extract the agent ID from a user-agent key.
///
/// # Panics
///
/// Panics if the key is not at least 64 bytes.
#[must_use]
pub fn extract_agent_id_from_user_agent_key(key: &[u8]) -> AgentId {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&key[32..64]);
    AgentId::from_bytes(bytes)
}

/// Encode a status-agent index key: `status || agent_id`.
///
/// This allows efficient prefix scans for all agents with a given status.
#[must_use]
pub fn status_agent_key(status: u8, agent_id: &AgentId) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(status);
    key.extend_from_slice(agent_id.as_bytes());
    key
}

/// Encode a status prefix for scanning all agents by status.
#[must_use]
pub fn status_prefix(status: u8) -> Vec<u8> {
    vec![status]
}

/// Encode a session key (just the session ID bytes).
#[must_use]
pub fn session_key(session_id: &SessionId) -> Vec<u8> {
    session_id.as_bytes().to_vec()
}

/// Encode an agent-session index key: `agent_id || session_id`.
///
/// This allows efficient prefix scans for all sessions belonging to an agent.
#[must_use]
pub fn agent_session_key(agent_id: &AgentId, session_id: &SessionId) -> Vec<u8> {
    let mut key = Vec::with_capacity(48);
    key.extend_from_slice(agent_id.as_bytes());
    key.extend_from_slice(session_id.as_bytes());
    key
}

/// Encode an agent prefix for scanning all sessions by agent.
#[must_use]
pub fn agent_prefix(agent_id: &AgentId) -> Vec<u8> {
    agent_id.as_bytes().to_vec()
}

/// Extract the session ID from an agent-session key.
///
/// # Panics
///
/// Panics if the key is not at least 48 bytes.
#[must_use]
pub fn extract_session_id_from_agent_session_key(key: &[u8]) -> SessionId {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&key[32..48]);
    SessionId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

/// Encode a user key (just the user ID bytes).
#[must_use]
pub fn user_key(user_id: &UserId) -> Vec<u8> {
    user_id.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_key_roundtrip() {
        let user_id = UserId::from_bytes([1u8; 32]);
        let agent_id = AgentId::from_bytes([2u8; 32]);

        let key = user_agent_key(&user_id, &agent_id);
        assert_eq!(key.len(), 64);

        let extracted = extract_agent_id_from_user_agent_key(&key);
        assert_eq!(extracted, agent_id);
    }

    #[test]
    fn agent_session_key_roundtrip() {
        let agent_id = AgentId::from_bytes([1u8; 32]);
        let session_id = SessionId::generate();

        let key = agent_session_key(&agent_id, &session_id);
        assert_eq!(key.len(), 48);

        let extracted = extract_session_id_from_agent_session_key(&key);
        assert_eq!(extracted, session_id);
    }

    #[test]
    fn prefix_scan_simulation() {
        let user_id = UserId::from_bytes([1u8; 32]);
        let agent_id1 = AgentId::from_bytes([2u8; 32]);
        let agent_id2 = AgentId::from_bytes([3u8; 32]);

        let key1 = user_agent_key(&user_id, &agent_id1);
        let key2 = user_agent_key(&user_id, &agent_id2);
        let prefix = user_prefix(&user_id);

        // Both keys should start with the user prefix
        assert!(key1.starts_with(&prefix));
        assert!(key2.starts_with(&prefix));
    }
}
