//! JWT validation and claims extraction.
//!
//! This module provides the core JWT validation logic, including signature
//! verification and claims validation.

use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::Deserialize;

use aura_swarm_core::UserId;

use crate::error::{AuthError, Result};
use crate::jwks::JwksProvider;
use crate::AuthConfig;

/// Validated claims extracted from a JWT.
#[derive(Debug, Clone)]
pub struct ValidatedClaims {
    /// The user ID extracted from the `sub` claim.
    pub user_id: UserId,
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
    /// Subject (`user_id` as UUID string)
    sub: String,
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
/// JWT signatures using EdDSA or RS256.
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
        let header = decode_header(token).map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        let kid = header
            .kid
            .ok_or_else(|| AuthError::MissingClaim("kid".to_string()))?;

        let key = self.jwks.get_key(&kid).await?;

        let algorithm = match header.alg {
            Algorithm::RS256 => Algorithm::RS256,
            Algorithm::EdDSA => Algorithm::EdDSA,
            other => {
                return Err(AuthError::InvalidToken(format!(
                    "unsupported algorithm: {other:?}"
                )))
            }
        };

        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[self.config.issuer()]);
        validation.validate_aud = false;
        validation.validate_exp = true;

        let token_data =
            decode::<RawClaims>(token, &key, &validation).map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                jsonwebtoken::errors::ErrorKind::InvalidIssuer => AuthError::InvalidIssuer,
                jsonwebtoken::errors::ErrorKind::InvalidSignature => AuthError::InvalidSignature,
                _ => AuthError::InvalidToken(e.to_string()),
            })?;

        let claims = token_data.claims;

        if let Some(ref audience) = self.config.audience {
            if !claims.aud.contains(audience) {
                return Err(AuthError::InvalidAudience);
            }
        }

        let user_id = UserId::from_str(&claims.sub).map_err(|_| AuthError::InvalidUserId)?;

        let exp_secs = i64::try_from(claims.exp).unwrap_or(i64::MAX);
        let expires_at = DateTime::from_timestamp(exp_secs, 0)
            .ok_or_else(|| AuthError::InvalidToken("invalid exp timestamp".to_string()))?;

        Ok(ValidatedClaims {
            user_id,
            expires_at,
        })
    }
}

/// A mock JWT validator for testing.
///
/// Accepts tokens in the format `test-token:<user-uuid>` and extracts the user ID.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockJwtValidator;

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockJwtValidator {
    fn default() -> Self {
        Self
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait]
impl JwtValidator for MockJwtValidator {
    async fn validate(&self, token: &str) -> Result<ValidatedClaims> {
        let uuid_str = token.strip_prefix("test-token:").ok_or_else(|| {
            AuthError::InvalidToken("expected test-token:<user-uuid>".to_string())
        })?;

        let user_id = UserId::from_str(uuid_str).map_err(|_| AuthError::InvalidUserId)?;

        Ok(ValidatedClaims {
            user_id,
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
        let user_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let token = format!("test-token:{user_uuid}");

        let claims = validator.validate(&token).await.unwrap();
        assert_eq!(claims.user_id.to_string(), user_uuid);
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

        let result = validator.validate("test-token:not-a-uuid").await;
        assert!(result.is_err());
    }
}
