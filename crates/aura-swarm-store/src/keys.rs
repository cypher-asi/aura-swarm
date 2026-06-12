//! Key encoding utilities for `RocksDB`.
//!
//! This module provides functions to encode and decode keys for various indexes.
//! All keys are designed to support efficient prefix scans.
//!
//! Key sizes (all IDs are UUID/16 bytes):
//! - `agent_key`: 16 bytes (AgentId)
//! - `user_agent_key`: 32 bytes (UserId + AgentId)
//! - `status_agent_key`: 17 bytes (1 byte status + AgentId)
//! - `agent_session_key`: 32 bytes (AgentId + SessionId)
//! - `user_key`: 16 bytes (UserId)
//! - `process_trigger_key`: 16 bytes + process id length (`AgentId` + UTF-8 process id)
//! - `usage_event_key`: 24 bytes (`AgentId` + big-endian `u64` timestamp millis)
//! - `log_snapshot_key`: 24 bytes (`AgentId` + big-endian `u64` captured-at millis)

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
    let mut key = Vec::with_capacity(32);
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
/// Panics if the key is not at least 32 bytes.
#[must_use]
pub fn extract_agent_id_from_user_agent_key(key: &[u8]) -> AgentId {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&key[16..32]);
    AgentId::from_bytes(bytes)
}

/// Encode a status-agent index key: `status || agent_id`.
///
/// This allows efficient prefix scans for all agents with a given status.
#[must_use]
pub fn status_agent_key(status: u8, agent_id: &AgentId) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
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
    let mut key = Vec::with_capacity(32);
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
/// Panics if the key is not at least 32 bytes.
#[must_use]
pub fn extract_session_id_from_agent_session_key(key: &[u8]) -> SessionId {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&key[16..32]);
    SessionId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

/// Encode a user key (just the user ID bytes).
#[must_use]
pub fn user_key(user_id: &UserId) -> Vec<u8> {
    user_id.as_bytes().to_vec()
}

/// Encode a process-trigger key: `agent_id || process_id` (UTF-8).
///
/// Use [`agent_prefix`] to scan all registered triggers for an agent.
#[must_use]
pub fn process_trigger_key(agent_id: &AgentId, process_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(16 + process_id.len());
    key.extend_from_slice(agent_id.as_bytes());
    key.extend_from_slice(process_id.as_bytes());
    key
}

/// Extract the process ID from a process-trigger key.
///
/// Returns `None` if the key is too short or the suffix is not valid UTF-8.
#[must_use]
pub fn extract_process_id_from_trigger_key(key: &[u8]) -> Option<String> {
    key.get(16..)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_owned)
}

/// Encode a usage-event key: `agent_id || timestamp_millis` (big-endian).
///
/// Big-endian millis keep events time-ordered under a prefix scan with
/// [`agent_prefix`].
#[must_use]
pub fn usage_event_key(agent_id: &AgentId, timestamp_millis: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(24);
    key.extend_from_slice(agent_id.as_bytes());
    key.extend_from_slice(&timestamp_millis.to_be_bytes());
    key
}

/// Encode a log-snapshot key: `agent_id || captured_at_millis` (big-endian).
///
/// Big-endian millis keep snapshots time-ordered under a prefix scan with
/// [`agent_prefix`], so the per-agent cap can prune the oldest entries.
#[must_use]
pub fn log_snapshot_key(agent_id: &AgentId, captured_at_millis: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(24);
    key.extend_from_slice(agent_id.as_bytes());
    key.extend_from_slice(&captured_at_millis.to_be_bytes());
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_key_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "test", 42);

        let key = user_agent_key(&user_id, &agent_id);
        assert_eq!(key.len(), 32);

        let extracted = extract_agent_id_from_user_agent_key(&key);
        assert_eq!(extracted, agent_id);
    }

    #[test]
    fn agent_session_key_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "test", 42);
        let session_id = SessionId::generate();

        let key = agent_session_key(&agent_id, &session_id);
        assert_eq!(key.len(), 32);

        let extracted = extract_session_id_from_agent_session_key(&key);
        assert_eq!(extracted, session_id);
    }

    #[test]
    fn prefix_scan_simulation() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id1 = AgentId::generate_deterministic(&user_id, "agent-1", 1);
        let agent_id2 = AgentId::generate_deterministic(&user_id, "agent-2", 2);

        let key1 = user_agent_key(&user_id, &agent_id1);
        let key2 = user_agent_key(&user_id, &agent_id2);
        let prefix = user_prefix(&user_id);

        // Both keys should start with the user prefix
        assert!(key1.starts_with(&prefix));
        assert!(key2.starts_with(&prefix));
    }

    #[test]
    fn status_agent_key_format() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "test", 42);

        let key = status_agent_key(2, &agent_id); // 2 = Running
        assert_eq!(key.len(), 17);
        assert_eq!(key[0], 2);
        assert_eq!(&key[1..17], agent_id.as_bytes());
    }

    #[test]
    fn process_trigger_key_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "test", 42);

        let key = process_trigger_key(&agent_id, "proc-123");
        assert!(key.starts_with(&agent_prefix(&agent_id)));
        assert_eq!(
            extract_process_id_from_trigger_key(&key).as_deref(),
            Some("proc-123")
        );
    }

    #[test]
    fn log_snapshot_key_is_time_ordered() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "test", 42);

        let k1 = log_snapshot_key(&agent_id, 1_000);
        let k2 = log_snapshot_key(&agent_id, 2_000);
        assert_eq!(k1.len(), 24);
        assert!(k1.starts_with(&agent_prefix(&agent_id)));
        assert!(k1 < k2, "earlier snapshots must sort first");
    }

    #[test]
    fn usage_event_key_is_time_ordered() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let agent_id = AgentId::generate_deterministic(&user_id, "test", 42);

        let k1 = usage_event_key(&agent_id, 1_000);
        let k2 = usage_event_key(&agent_id, 2_000);
        assert_eq!(k1.len(), 24);
        assert!(k1.starts_with(&agent_prefix(&agent_id)));
        assert!(k1 < k2, "earlier events must sort first");
    }
}
