//! Core identifier types for aura-swarm.
//!
//! This module provides strongly-typed identifiers for users, agents, and sessions.
//! All IDs are designed for efficient storage and lookup.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A 32-byte user identifier, hex-encoded for display.
///
/// User IDs are provided by Zero-ID and extracted from JWT `sub` claims.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId([u8; 32]);

impl UserId {
    /// Create a new `UserId` from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse a `UserId` from a hex-encoded string.
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not valid hex or not exactly 64 characters.
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        let bytes = hex::decode(s).map_err(|_| IdError::InvalidHex)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| IdError::InvalidLength {
            expected: 32,
            got: s.len() / 2,
        })?;
        Ok(Self(arr))
    }

    /// Return the underlying bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the hex-encoded string representation.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserId({})", self.to_hex())
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl TryFrom<String> for UserId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_hex(&value)
    }
}

impl From<UserId> for String {
    fn from(id: UserId) -> Self {
        id.to_hex()
    }
}

impl AsRef<[u8]> for UserId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A 32-byte agent identifier, generated via blake3 hash.
///
/// Agent IDs are deterministically generated from `user_id`, `name`, and a timestamp
/// to ensure uniqueness while allowing reproducible generation in tests.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId([u8; 32]);

impl AgentId {
    /// Create a new `AgentId` from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Generate a new unique `AgentId` using blake3.
    ///
    /// The ID is derived from the user ID, agent name, and current timestamp.
    #[must_use]
    pub fn generate(user_id: &UserId, name: &str) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        let mut hasher = blake3::Hasher::new();
        hasher.update(user_id.as_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&timestamp.to_le_bytes());

        Self(*hasher.finalize().as_bytes())
    }

    /// Generate a deterministic `AgentId` for testing.
    ///
    /// This is useful for creating predictable IDs in tests.
    #[must_use]
    pub fn generate_deterministic(user_id: &UserId, name: &str, seed: u64) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(user_id.as_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&seed.to_le_bytes());

        Self(*hasher.finalize().as_bytes())
    }

    /// Parse an `AgentId` from a hex-encoded string.
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not valid hex or not exactly 64 characters.
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        let bytes = hex::decode(s).map_err(|_| IdError::InvalidHex)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| IdError::InvalidLength {
            expected: 32,
            got: s.len() / 2,
        })?;
        Ok(Self(arr))
    }

    /// Return the underlying bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the hex-encoded string representation.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgentId({})", self.to_hex())
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
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
        id.to_hex()
    }
}

impl AsRef<[u8]> for AgentId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A 16-byte session identifier based on UUID v4.
///
/// Session IDs are randomly generated for each new session.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(uuid::Uuid);

/// ZID identity ID (UUID format).
///
/// This represents the user's identity in Zero-ID, extracted from JWT `sub` claims.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IdentityId(uuid::Uuid);

/// ZID namespace/tenant ID (UUID format).
///
/// This represents the tenant context in Zero-ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NamespaceId(uuid::Uuid);

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

impl IdentityId {
    /// Create a new `IdentityId` from a UUID.
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
}

impl FromStr for IdentityId {
    type Err = IdError;

    /// Parse an `IdentityId` from a UUID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = uuid::Uuid::parse_str(s).map_err(|_| IdError::InvalidUuid)?;
        Ok(Self(uuid))
    }
}

impl fmt::Debug for IdentityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityId({})", self.0)
    }
}

impl fmt::Display for IdentityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for IdentityId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<IdentityId> for String {
    fn from(id: IdentityId) -> Self {
        id.0.to_string()
    }
}

impl AsRef<[u8]> for IdentityId {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl NamespaceId {
    /// Create a new `NamespaceId` from a UUID.
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
}

impl FromStr for NamespaceId {
    type Err = IdError;

    /// Parse a `NamespaceId` from a UUID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = uuid::Uuid::parse_str(s).map_err(|_| IdError::InvalidUuid)?;
        Ok(Self(uuid))
    }
}

impl fmt::Debug for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NamespaceId({})", self.0)
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for NamespaceId {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<NamespaceId> for String {
    fn from(id: NamespaceId) -> Self {
        id.0.to_string()
    }
}

impl AsRef<[u8]> for NamespaceId {
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
    fn user_id_roundtrip() {
        let bytes = [0x42u8; 32];
        let id = UserId::from_bytes(bytes);
        let hex = id.to_hex();
        let parsed = UserId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn user_id_invalid_hex() {
        let result = UserId::from_hex("not-valid-hex");
        assert!(matches!(result, Err(IdError::InvalidHex)));
    }

    #[test]
    fn user_id_wrong_length() {
        let result = UserId::from_hex("deadbeef");
        assert!(matches!(result, Err(IdError::InvalidLength { .. })));
    }

    #[test]
    fn agent_id_deterministic() {
        let user_id = UserId::from_bytes([1u8; 32]);
        let id1 = AgentId::generate_deterministic(&user_id, "test-agent", 123);
        let id2 = AgentId::generate_deterministic(&user_id, "test-agent", 123);
        assert_eq!(id1, id2);

        let id3 = AgentId::generate_deterministic(&user_id, "test-agent", 456);
        assert_ne!(id1, id3);
    }

    #[test]
    fn agent_id_unique() {
        let user_id = UserId::from_bytes([1u8; 32]);
        let id1 = AgentId::generate(&user_id, "test-agent");
        let id2 = AgentId::generate(&user_id, "test-agent");
        // Due to timestamp, these should be different (with high probability)
        assert_ne!(id1, id2);
    }

    #[test]
    fn session_id_roundtrip() {
        let id = SessionId::generate();
        let str_repr = id.to_string();
        let parsed = SessionId::from_str(&str_repr).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn user_id_serde_json() {
        let bytes = [0xab; 32];
        let id = UserId::from_bytes(bytes);
        let json = serde_json::to_string(&id).unwrap();
        let parsed: UserId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_id_serde_json() {
        let user_id = UserId::from_bytes([1u8; 32]);
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
    fn identity_id_roundtrip() {
        let uuid = uuid::Uuid::new_v4();
        let id = IdentityId::from_uuid(uuid);
        let str_repr = id.to_string();
        let parsed = IdentityId::from_str(&str_repr).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn identity_id_serde_json() {
        let id = IdentityId::from_uuid(uuid::Uuid::new_v4());
        let json = serde_json::to_string(&id).unwrap();
        let parsed: IdentityId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn identity_id_invalid_uuid() {
        let result = IdentityId::from_str("not-a-uuid");
        assert!(matches!(result, Err(IdError::InvalidUuid)));
    }

    #[test]
    fn namespace_id_roundtrip() {
        let uuid = uuid::Uuid::new_v4();
        let id = NamespaceId::from_uuid(uuid);
        let str_repr = id.to_string();
        let parsed = NamespaceId::from_str(&str_repr).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn namespace_id_serde_json() {
        let id = NamespaceId::from_uuid(uuid::Uuid::new_v4());
        let json = serde_json::to_string(&id).unwrap();
        let parsed: NamespaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn namespace_id_invalid_uuid() {
        let result = NamespaceId::from_str("not-a-uuid");
        assert!(matches!(result, Err(IdError::InvalidUuid)));
    }
}
