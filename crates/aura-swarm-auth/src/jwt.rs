//! JWT validation and claims extraction.
//!
//! This module provides the core JWT validation logic, including signature
//! verification and claims validation.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::str::FromStr;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use parking_lot::RwLock;
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
    /// Issuer — not read directly but must be present for serde
    /// deserialization; validated by the `jsonwebtoken` crate.
    #[allow(dead_code)]
    iss: String,
    /// Subject (`user_id` as UUID string)
    sub: String,
    /// Audience (can be string or array)
    #[serde(default)]
    aud: Audience,
    /// Expiration timestamp
    exp: u64,
    /// Issued-at — not read directly but must be present for serde
    /// deserialization; validated by the `jsonwebtoken` crate.
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
    ///
    /// # Errors
    ///
    /// Returns `AuthError::Internal` if the HTTP client cannot be created.
    pub fn new(config: AuthConfig) -> Result<Self> {
        let jwks = JwksProvider::new(config.clone())?;
        Ok(Self { config, jwks })
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

/// Token validator that introspects tokens via the zOS user info API.
///
/// zOS uses HS256 (shared-secret) JWTs and does not expose a JWKS endpoint.
/// This validator calls `GET /api/users/current` with the bearer token to
/// confirm validity, then extracts the user ID from the response. Results
/// are cached to avoid hitting zOS on every request.
pub struct ZosTokenValidator {
    config: AuthConfig,
    client: reqwest::Client,
    cache: RwLock<HashMap<u64, CachedValidation>>,
    cache_ttl: Duration,
}

struct CachedValidation {
    claims: ValidatedClaims,
    validated_at: Instant,
}

/// Claims decoded from the JWT payload without signature verification,
/// used only to read the `exp` timestamp.
#[derive(Debug, Deserialize)]
struct UnsafeMinimalClaims {
    #[allow(dead_code)]
    sub: String,
    exp: u64,
}

impl ZosTokenValidator {
    /// Create a new zOS token validator.
    ///
    /// `cache_ttl` controls how long a successfully validated token is
    /// trusted before re-checking with zOS.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::Internal` if the HTTP client cannot be created.
    pub fn new(config: AuthConfig, cache_ttl: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AuthError::Internal(format!("failed to create HTTP client: {e}")))?;

        Ok(Self {
            config,
            client,
            cache: RwLock::new(HashMap::new()),
            cache_ttl,
        })
    }

    fn token_hash(token: &str) -> u64 {
        let mut h = DefaultHasher::new();
        token.hash(&mut h);
        h.finish()
    }
}

#[async_trait]
impl JwtValidator for ZosTokenValidator {
    async fn validate(&self, token: &str) -> Result<ValidatedClaims> {
        let key = Self::token_hash(token);

        // Check cache
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&key) {
                if entry.validated_at.elapsed() < self.cache_ttl {
                    return Ok(entry.claims.clone());
                }
            }
        }

        // Call zOS to validate
        let url = self.config.user_info_url();
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| AuthError::Internal(format!("zOS introspection failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            return match status.as_u16() {
                401 => Err(AuthError::InvalidToken("token rejected by zOS".to_string())),
                code => Err(AuthError::ZosApi {
                    status: code,
                    code: "INTROSPECTION_FAILED".to_string(),
                    message: format!("zOS returned HTTP {code}"),
                }),
            };
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AuthError::Internal(format!("invalid zOS response: {e}")))?;

        let user_id_str = body
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::Internal("zOS response missing 'id' field".to_string()))?;

        let user_id =
            UserId::from_str(user_id_str).map_err(|_| AuthError::InvalidUserId)?;

        // Decode exp from JWT payload (no signature check — zOS already validated)
        let expires_at = {
            let mut insecure = Validation::default();
            insecure.insecure_disable_signature_validation();
            insecure.validate_exp = false;
            insecure.validate_aud = false;
            insecure.required_spec_claims.clear();
            match decode::<UnsafeMinimalClaims>(
                token,
                &jsonwebtoken::DecodingKey::from_secret(b""),
                &insecure,
            ) {
                Ok(data) => {
                    let secs = i64::try_from(data.claims.exp).unwrap_or(i64::MAX);
                    DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
                }
                Err(_) => Utc::now() + chrono::Duration::hours(1),
            }
        };

        let claims = ValidatedClaims {
            user_id,
            expires_at,
        };

        // Update cache
        {
            let mut cache = self.cache.write();
            cache.insert(
                key,
                CachedValidation {
                    claims: claims.clone(),
                    validated_at: Instant::now(),
                },
            );

            // Evict stale entries periodically
            if cache.len() > 1000 {
                let ttl = self.cache_ttl;
                cache.retain(|_, v| v.validated_at.elapsed() < ttl);
            }
        }

        Ok(claims)
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

    #[test]
    fn audience_single_contains() {
        assert!(Audience::Single("test".into()).contains("test"));
    }

    #[test]
    fn audience_single_not_contains() {
        assert!(!Audience::Single("test".into()).contains("other"));
    }

    #[test]
    fn audience_multiple_contains() {
        assert!(Audience::Multiple(vec!["a".into(), "b".into()]).contains("b"));
    }

    #[test]
    fn audience_none_contains() {
        assert!(!Audience::None.contains("test"));
    }
}
