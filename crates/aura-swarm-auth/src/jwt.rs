//! JWT validation and claims extraction.
//!
//! This module provides the core JWT validation logic, including signature
//! verification and claims validation.

use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::Deserialize;

use aura_swarm_core::{IdentityId, NamespaceId, SessionId};

use crate::error::{AuthError, Result};
use crate::jwks::JwksProvider;
use crate::AuthConfig;

/// Validated claims extracted from a JWT.
#[derive(Debug, Clone)]
pub struct ValidatedClaims {
    /// The identity ID extracted from the `sub` claim (ZID UUID).
    pub identity_id: IdentityId,
    /// The namespace/tenant ID.
    pub namespace_id: NamespaceId,
    /// The session ID.
    pub session_id: SessionId,
    /// Whether MFA has been verified for this session.
    pub mfa_verified: bool,
    /// When the token expires.
    pub expires_at: DateTime<Utc>,
}

/// Trait for validating JWTs.
#[async_trait]
pub trait JwtValidator: Send + Sync {
    /// Validate a JWT and extract claims.
    ///
    /// # Errors
    ///
    /// Returns an error if the token is invalid, expired, or cannot be validated.
    async fn validate(&self, token: &str) -> Result<ValidatedClaims>;
}

/// Raw claims from a JWT before validation.
#[derive(Debug, Deserialize)]
struct RawClaims {
    /// Issuer (validated by jsonwebtoken)
    #[allow(dead_code)]
    iss: String,
    /// Subject (`identity_id` as UUID string)
    sub: String,
    /// Namespace/tenant ID
    namespace_id: String,
    /// Session ID
    session_id: String,
    /// MFA completion status
    #[serde(default)]
    mfa_verified: bool,
    /// Audience (can be string or array)
    #[serde(default)]
    aud: Audience,
    /// Expiration timestamp
    exp: u64,
    /// Issued at timestamp (validated by jsonwebtoken)
    #[allow(dead_code)]
    iat: u64,
}

/// Audience claim that can be either a string or array.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum Audience {
    Single(String),
    Multiple(Vec<String>),
    #[default]
    None,
}

impl Audience {
    fn contains(&self, value: &str) -> bool {
        match self {
            Self::Single(s) => s == value,
            Self::Multiple(v) => v.iter().any(|s| s == value),
            Self::None => false,
        }
    }
}

/// JWKS-based JWT validator.
///
/// This validator fetches public keys from a JWKS endpoint and validates
/// JWT signatures using Ed25519 (`EdDSA`).
pub struct JwksValidator {
    config: AuthConfig,
    jwks: JwksProvider,
}

impl JwksValidator {
    /// Create a new JWKS-based validator.
    #[must_use]
    pub fn new(config: AuthConfig) -> Self {
        let jwks = JwksProvider::new(config.clone());
        Self { config, jwks }
    }

    /// Get a reference to the JWKS provider for manual operations.
    #[must_use]
    pub const fn jwks(&self) -> &JwksProvider {
        &self.jwks
    }
}

#[async_trait]
impl JwtValidator for JwksValidator {
    async fn validate(&self, token: &str) -> Result<ValidatedClaims> {
        // Decode header to get key ID
        let header = decode_header(token).map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        let kid = header
            .kid
            .ok_or_else(|| AuthError::MissingClaim("kid".to_string()))?;

        // Get the decoding key
        let key = self.jwks.get_key(&kid).await?;

        // Set up validation
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[self.config.issuer()]);
        // We'll validate audience manually since it can be string or array
        validation.validate_aud = false;
        validation.validate_exp = true;

        // Decode and validate
        let token_data =
            decode::<RawClaims>(token, &key, &validation).map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                jsonwebtoken::errors::ErrorKind::InvalidIssuer => AuthError::InvalidIssuer,
                jsonwebtoken::errors::ErrorKind::InvalidSignature => AuthError::InvalidSignature,
                _ => AuthError::InvalidToken(e.to_string()),
            })?;

        let claims = token_data.claims;

        // Validate audience manually
        if !claims.aud.contains(&self.config.audience) {
            return Err(AuthError::InvalidAudience);
        }

        // Extract identity_id from sub (UUID format)
        let identity_id =
            IdentityId::from_str(&claims.sub).map_err(|_| AuthError::InvalidIdentityId)?;

        // Extract namespace_id (UUID format)
        let namespace_id = NamespaceId::from_str(&claims.namespace_id)
            .map_err(|_| AuthError::InvalidNamespaceId)?;

        // Extract session_id (UUID format)
        let session_id =
            SessionId::from_str(&claims.session_id).map_err(|_| AuthError::InvalidSessionId)?;

        // Convert expiration timestamp
        let exp_secs = i64::try_from(claims.exp).unwrap_or(i64::MAX);
        let expires_at = DateTime::from_timestamp(exp_secs, 0)
            .ok_or_else(|| AuthError::InvalidToken("invalid exp timestamp".to_string()))?;

        Ok(ValidatedClaims {
            identity_id,
            namespace_id,
            session_id,
            mfa_verified: claims.mfa_verified,
            expires_at,
        })
    }
}

/// A mock JWT validator for testing.
///
/// This validator accepts any token in the format `test-token:<identity_uuid>:<namespace_uuid>`
/// and extracts the IDs from it.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockJwtValidator {
    /// Whether MFA is verified for all validated tokens.
    pub mfa_verified: bool,
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockJwtValidator {
    fn default() -> Self {
        Self {
            mfa_verified: false,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl JwtValidator for MockJwtValidator {
    async fn validate(&self, token: &str) -> Result<ValidatedClaims> {
        // Expected format: test-token:<identity_uuid>:<namespace_uuid>
        let rest = token.strip_prefix("test-token:").ok_or_else(|| {
            AuthError::InvalidToken("expected test-token:<identity>:<namespace>".to_string())
        })?;

        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() != 2 {
            return Err(AuthError::InvalidToken(
                "expected test-token:<identity>:<namespace>".to_string(),
            ));
        }

        let identity_id =
            IdentityId::from_str(parts[0]).map_err(|_| AuthError::InvalidIdentityId)?;
        let namespace_id =
            NamespaceId::from_str(parts[1]).map_err(|_| AuthError::InvalidNamespaceId)?;
        let session_id = SessionId::generate();

        Ok(ValidatedClaims {
            identity_id,
            namespace_id,
            session_id,
            mfa_verified: self.mfa_verified,
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_validator_works() {
        let validator = MockJwtValidator::default();
        let identity_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let namespace_uuid = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        let token = format!("test-token:{identity_uuid}:{namespace_uuid}");

        let claims = validator.validate(&token).await.unwrap();
        assert_eq!(claims.identity_id.to_string(), identity_uuid);
        assert_eq!(claims.namespace_id.to_string(), namespace_uuid);
        assert!(!claims.mfa_verified);
    }

    #[tokio::test]
    async fn mock_validator_with_mfa() {
        let validator = MockJwtValidator { mfa_verified: true };
        let identity_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let namespace_uuid = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        let token = format!("test-token:{identity_uuid}:{namespace_uuid}");

        let claims = validator.validate(&token).await.unwrap();
        assert!(claims.mfa_verified);
    }

    #[tokio::test]
    async fn mock_validator_rejects_invalid() {
        let validator = MockJwtValidator::default();

        let result = validator.validate("invalid-token").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mock_validator_rejects_malformed_uuid() {
        let validator = MockJwtValidator::default();

        let result = validator
            .validate("test-token:not-a-uuid:also-not-uuid")
            .await;
        assert!(result.is_err());
    }
}
