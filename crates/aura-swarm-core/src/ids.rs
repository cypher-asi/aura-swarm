//! Core identifier types for aura-swarm.
//!
//! This module provides strongly-typed identifiers for users, agents, and sessions.
//! All IDs are based on UUIDs for efficient storage and compatibility with zOS.

use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;
use std::str::FromStr;

/// A 16-byte agent identifier based on UUID.
///
/// Agent IDs are deterministically generated from `user_id`, `name`, and a timestamp
/// using HKDF to ensure uniqueness while allowing reproducible generation in tests.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId(uuid::Uuid);

impl AgentId {
    /// Create a new `AgentId` from a UUID.
    #[must_use]
    pub const fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    /// Create a new `AgentId` from raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(uuid::Uuid::from_bytes(bytes))
    }

    /// Generate a new unique `AgentId` as a random UUID v4.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// Generate a deterministic `AgentId` for testing.
    ///
    /// This is useful for creating predictable IDs in tests.
    #[must_use]
    pub fn generate_deterministic(user_id: &UserId, name: &str, seed: u64) -> Self {
        Self::generate_with_timestamp(user_id, name, u128::from(seed))
    }

    /// Internal helper to generate an AgentId with a specific timestamp/seed.
    fn generate_with_timestamp(user_id: &UserId, name: &str, timestamp: u128) -> Self {
        let mut ikm = Vec::new();
        ikm.extend_from_slice(user_id.as_bytes());
        ikm.extend_from_slice(name.as_bytes());
        ikm.extend_from_slice(&timestamp.to_le_bytes());

        let hk = Hkdf::<Sha256>::new(None, &ikm);
        let mut okm = [0u8; 16];
        hk.expand(b"aura:agent-id:v1", &mut okm)
            .expect("16 bytes is valid output length for HKDF-SHA256");

        Self(uuid::Uuid::from_bytes(okm))
    }

    /// Parse an `AgentId` from a UUID string.
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not a valid UUID.
    pub fn from_str(s: &str) -> Result<Self, IdError> {
        let uuid = uuid::Uuid::parse_str(s).map_err(|_| IdError::InvalidUuid)?;
        Ok(Self(uuid))
    }

    /// Parse an `AgentId` from a hex-encoded string.
    ///
    /// Supports both UUID format (with hyphens) and raw hex (32 chars).
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not valid hex or not the correct length.
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        // Try UUID format first (contains hyphens)
        if s.contains('-') {
            return Self::from_str(s);
        }

        // Otherwise try raw hex (32 chars = 16 bytes)
        let bytes = hex::decode(s).map_err(|_| IdError::InvalidHex)?;
        let len = bytes.len();
        let arr: [u8; 16] = bytes.try_into().map_err(|_| IdError::InvalidLength {
            expected: 16,
            got: len,
        })?;
        Ok(Self(uuid::Uuid::from_bytes(arr)))
    }

    /// Return the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &uuid::Uuid {
        &self.0
    }

    /// Return the bytes of the UUID.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Return the hex-encoded string representation (no hyphens).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0.as_bytes())
    }
}

impl fmt::Debug for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgentId({})", self.0)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for AgentId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_hex(&value)
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> Self {
        id.0.to_string()
    }
}

impl AsRef<[u8]> for AgentId {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// A 16-byte session identifier based on UUID v4.
///
/// Session IDs are randomly generated for each new session.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(uuid::Uuid);

/// zOS user ID (UUID format).
///
/// This represents the user's identity in zOS, extracted from JWT `sub` claims.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId(uuid::Uuid);

impl SessionId {
    /// Create a new `SessionId` from a UUID.
    #[must_use]
    pub const fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    /// Generate a new random `SessionId`.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// Return the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &uuid::Uuid {
        &self.0
    }

    /// Return the bytes of the UUID.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl FromStr for SessionId {
    type Err = IdError;

    /// Parse a `SessionId` from a UUID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = uuid::Uuid::parse_str(s).map_err(|_| IdError::InvalidUuid)?;
        Ok(Self(uuid))
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({})", self.0)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for SessionId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0.to_string()
    }
}

impl AsRef<[u8]> for SessionId {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl UserId {
    /// Create a new `UserId` from a UUID.
    #[must_use]
    pub const fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    /// Return the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &uuid::Uuid {
        &self.0
    }

    /// Return the bytes of the UUID.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Return the hex-encoded string representation (no hyphens).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0.as_bytes())
    }
}

impl FromStr for UserId {
    type Err = IdError;

    /// Parse a `UserId` from a UUID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = uuid::Uuid::parse_str(s).map_err(|_| IdError::InvalidUuid)?;
        Ok(Self(uuid))
    }
}

impl fmt::Debug for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserId({})", self.0)
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for UserId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<UserId> for String {
    fn from(id: UserId) -> Self {
        id.0.to_string()
    }
}

impl AsRef<[u8]> for UserId {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Errors that can occur when parsing identifiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The input string contains invalid hexadecimal characters.
    #[error("invalid hex encoding")]
    InvalidHex,

    /// The input has an incorrect length.
    #[error("invalid length: expected {expected} bytes, got {got}")]
    InvalidLength {
        /// The expected number of bytes.
        expected: usize,
        /// The actual number of bytes.
        got: usize,
    },

    /// The input is not a valid UUID.
    #[error("invalid UUID format")]
    InvalidUuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_deterministic() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let id1 = AgentId::generate_deterministic(&user_id, "test-agent", 123);
        let id2 = AgentId::generate_deterministic(&user_id, "test-agent", 123);
        assert_eq!(id1, id2);

        let id3 = AgentId::generate_deterministic(&user_id, "test-agent", 456);
        assert_ne!(id1, id3);
    }

    #[test]
    fn agent_id_unique() {
        let id1 = AgentId::generate();
        let id2 = AgentId::generate();
        assert_ne!(id1, id2);
    }

    #[test]
    fn agent_id_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let id = AgentId::generate_deterministic(&user_id, "test", 42);
        let str_repr = id.to_string();
        let parsed = AgentId::from_str(&str_repr).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_id_hex_roundtrip() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let id = AgentId::generate_deterministic(&user_id, "test", 42);
        let hex = id.to_hex();
        let parsed = AgentId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_id_roundtrip() {
        let id = SessionId::generate();
        let str_repr = id.to_string();
        let parsed = SessionId::from_str(&str_repr).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_id_serde_json() {
        let user_id = UserId::from_uuid(uuid::Uuid::new_v4());
        let id = AgentId::generate_deterministic(&user_id, "test", 42);
        let json = serde_json::to_string(&id).unwrap();
        let parsed: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_id_serde_json() {
        let id = SessionId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn user_id_roundtrip() {
        let uuid = uuid::Uuid::new_v4();
        let id = UserId::from_uuid(uuid);
        let str_repr = id.to_string();
        let parsed = UserId::from_str(&str_repr).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn user_id_serde_json() {
        let id = UserId::from_uuid(uuid::Uuid::new_v4());
        let json = serde_json::to_string(&id).unwrap();
        let parsed: UserId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn user_id_invalid_uuid() {
        let result = UserId::from_str("not-a-uuid");
        assert!(matches!(result, Err(IdError::InvalidUuid)));
    }

    #[test]
    fn agent_id_from_invalid_hex_odd_length() {
        let result = AgentId::from_hex("abc");
        assert!(result.is_err());
    }

    #[test]
    fn agent_id_from_invalid_hex_non_hex() {
        let result = AgentId::from_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz");
        assert!(result.is_err());
    }

    #[test]
    fn session_id_from_invalid_string() {
        let result = SessionId::from_str("not-a-uuid");
        assert!(result.is_err());
    }

    #[test]
    fn user_id_display_matches_parse() {
        let id = UserId::from_uuid(uuid::Uuid::new_v4());
        let display = id.to_string();
        let parsed = UserId::from_str(&display).unwrap();
        assert_eq!(id, parsed);
    }
}
